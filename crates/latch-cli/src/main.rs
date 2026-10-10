#![forbid(unsafe_code)]

mod cli;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod web;

use anyhow::{Result, anyhow, bail};
use clap::Parser;
use cli::command::{Args, Commands, DebugCommand};
use cli::interactive::{InteractiveOutcome, run_tui};
use cli::session::{
    BuiltSession, ProfileOverrides, SelectedSession, SessionRequest, build_agent,
    ingest_attachments, pick_session, select_session,
};
use latch_kernel::{Config, prompt::PromptCompiler};
use latch_protocol::Mode;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let mut args = Args::parse();
    if args.command.is_some()
        && (args.web
            || args.resume
            || args.latest
            || args.session.is_some()
            || args.prompt.is_some())
    {
        // The top-level one-shot/resume flags describe the compatibility path;
        // mixing them with an explicit machine command is ambiguous.
        eprintln!(
            "error: --web/--resume/--session/--latest/-p apply to the interactive path; \
             use `latch run` or `latch resume` for machine commands"
        );
        return ExitCode::from(2);
    }
    match args.command.take() {
        Some(Commands::Run(run)) => {
            cli::machine::execute(cli::machine::run_request(&args, run)).await
        }
        Some(Commands::Resume(resume)) => {
            cli::machine::execute(cli::machine::resume_request(&args, resume)).await
        }
        Some(Commands::Sessions(sessions)) => cli::sessions::execute(&args, sessions).await,
        Some(Commands::Skills) => legacy_exit(cli::integrations::skills()),
        Some(Commands::Mcp { check }) => legacy_exit(cli::integrations::mcp(&args, check).await),
        Some(Commands::Doctor(doctor)) => cli::doctor::execute(&args, doctor),
        Some(Commands::Migrate) => legacy_exit(migrate(&args)),
        Some(Commands::Debug { command }) => legacy_exit(debug_dispatch(&args, command)),
        None => legacy_exit(run_legacy(args).await),
    }
}

fn migrate(args: &Args) -> Result<ExitCode> {
    if args.config.is_some() {
        bail!("latch migrate uses the discovered legacy XDG configuration; omit --config");
    }
    let source = latch_kernel::paths::ResolvedPaths::resolve(None, None);
    let target = latch_kernel::paths::ResolvedPaths::new_destination();
    latch_kernel::migration::migrate_legacy(&source, &target)?;
    println!("Migrated Latch storage to {}", target.state_root.display());
    Ok(ExitCode::SUCCESS)
}

fn legacy_exit(result: Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::from(1)
        }
    }
}

fn debug_dispatch(args: &Args, command: DebugCommand) -> Result<ExitCode> {
    match command {
        DebugCommand::Prompt { fragment } => {
            let workspace = std::env::current_dir()?;
            debug_prompt(
                &workspace,
                args.mode.unwrap_or(Mode::Work),
                fragment.as_deref(),
            )?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn run_legacy(args: Args) -> Result<ExitCode> {
    if args.web {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        return web::run(args).await;
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        bail!("the Web server supports Linux and Windows; use the TUI on this platform");
    }
    if let Some(prompt) = args.prompt.clone() {
        // Compatibility path into the machine run implementation: one prompt,
        // streamed as text, with the same session/provider construction.
        return Ok(cli::machine::execute(cli::machine::legacy_request(&args, prompt)).await);
    }
    let config = Config::load(args.config.as_deref())?;
    let mut workspace = std::env::current_dir()?.canonicalize()?;
    let overrides = profile_overrides(&args);
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive {
        anyhow::bail!("interactive mode requires a terminal; use `latch run` for piped input");
    }
    let mut selected = match select_session(
        &workspace,
        &config,
        &legacy_session_request(&args, !interactive),
    )
    .await?
    {
        SelectedSession::Session(id, session_workspace) => {
            workspace = session_workspace;
            Some(id)
        }
        SelectedSession::Fresh => None,
        SelectedSession::Exit => return Ok(ExitCode::SUCCESS),
    };
    let mut built =
        build_agent_interruptible(&workspace, &config, &overrides, selected, interactive).await?;
    loop {
        let attachments = ingest_attachments(&config, built.agent.session_id, &args.attach)?;
        match run_tui(built, workspace.clone(), attachments).await? {
            InteractiveOutcome::Exit => return Ok(ExitCode::SUCCESS),
            InteractiveOutcome::Resume => {
                // `/resume` always opens the picker, exactly like before.
                selected = match pick_session(&workspace, &config).await? {
                    SelectedSession::Session(id, session_workspace) => {
                        workspace = session_workspace;
                        Some(id)
                    }
                    SelectedSession::Fresh => None,
                    SelectedSession::Exit => return Ok(ExitCode::SUCCESS),
                };
                built = build_agent_interruptible(&workspace, &config, &overrides, selected, true)
                    .await?;
            }
        }
    }
}

fn legacy_session_request(args: &Args, non_interactive: bool) -> SessionRequest {
    SessionRequest {
        resume: args.resume,
        session: args.session.clone(),
        latest: args.latest,
        non_interactive,
    }
}

fn profile_overrides(args: &Args) -> ProfileOverrides {
    ProfileOverrides {
        mode: args.mode,
        provider: args.provider.clone(),
        model: args.model.clone(),
        effort: args.effort.clone(),
        config_path: args.config.clone(),
    }
}

/// Builds one session with Ctrl+C wired to startup cancellation. Extension
/// startup is bounded and cancellable, so a hanging extension cannot block the
/// interactive path before the TUI exists. The watcher is scoped to
/// construction: once the TUI owns the terminal its own input handling is
/// authoritative, and SIGTERM behavior is deliberately unchanged.
async fn build_agent_interruptible(
    workspace: &Path,
    config: &Config,
    overrides: &ProfileOverrides,
    selected: Option<Uuid>,
    interactive: bool,
) -> Result<BuiltSession> {
    let cancel = CancellationToken::new();
    let ctrl_c = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        }
    });
    let result = build_agent(workspace, config, overrides, selected, interactive, &cancel).await;
    ctrl_c.abort();
    Ok(result?)
}

fn debug_prompt(workspace: &Path, mode: Mode, id: Option<&str>) -> Result<()> {
    let p = PromptCompiler::compile(mode, workspace)?;
    let estimator = latch_kernel::TokenEstimator::generic();
    if let Some(id) = id {
        let f = p
            .fragment(id)
            .ok_or_else(|| anyhow!("unknown fragment {id}"))?;
        println!(
            "[{} v{} priority={} cacheable={}]\n{}",
            f.id, f.version, f.priority, f.cacheable, f.content
        );
    } else {
        println!(
            "fragments: {}  stable ≈{}  session ≈{}  total ≈{}\n",
            p.fragments.len(),
            estimator.estimate(&p.stable),
            estimator.estimate(&p.session),
            estimator.estimate(&p.text)
        );
        for f in &p.fragments {
            println!(
                "{:>4}  {:<38} v{}  ≈{} tokens",
                f.priority,
                f.id,
                f.version,
                estimator.estimate(&f.content)
            );
        }
        println!(
            "\n=== stable system (session-independent) ===\n{}",
            p.stable
        );
        println!(
            "\n=== session context (first provider-visible message) ===\n{}",
            p.session
        );
    }
    Ok(())
}
