#![forbid(unsafe_code)]

mod cli;

use anyhow::{Result, anyhow, bail};
use clap::Parser;
use cli::command::{Args, Commands, DebugCommand};
use cli::session::{
    BuiltSession, InferenceContext, ProfileOverrides, Restored, SelectedSession, SessionInfo,
    SessionRequest, build_agent, ingest_attachments, pick_session, select_session,
};
use latch_kernel::{
    Agent, Config, CredentialRef, ModelDescriptor, ProviderRegistry,
    agent::{AgentOutput, SteeringSubmission},
    config::{InferenceConfig, ProviderKind},
    prompt::PromptCompiler,
};
use latch_protocol::{InferenceProfile, MediaRef, Mode, ReasoningEffort, StreamEvent, UserInput};
use latch_tui::{Input, Output, SLASH_COMMANDS, SetupCredential, SetupPlan};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
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
        && (args.resume || args.latest || args.session.is_some() || args.prompt.is_some())
    {
        // The top-level one-shot/resume flags describe the compatibility path;
        // mixing them with an explicit machine command is ambiguous.
        eprintln!(
            "error: --resume/--session/--latest/-p apply to the interactive path; \
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
    if let Some(prompt) = args.prompt.clone() {
        // Compatibility path into the machine run implementation: one prompt,
        // streamed as text, with the same session/provider construction.
        return Ok(cli::machine::execute(cli::machine::legacy_request(&args, prompt)).await);
    }
    let config = Config::load(args.config.as_deref())?;
    let mut workspace = std::env::current_dir()?;
    let overrides = profile_overrides(&args);
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
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
        match interactive_session(
            built.agent,
            built.info,
            built.context,
            built.restored,
            workspace.clone(),
            attachments,
        )
        .await?
        {
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

enum InteractiveOutcome {
    Exit,
    Resume,
}

async fn interactive_session(
    mut agent: Agent,
    info: SessionInfo,
    mut context: InferenceContext,
    restored: Option<Restored>,
    workspace: PathBuf,
    initial_attachments: Vec<MediaRef>,
) -> Result<InteractiveOutcome> {
    // The TUI is the only path that can approve `Ask` policy decisions.
    agent.enable_interactive_permissions();
    let broker = agent.permission_broker();
    let steering = agent.steering_handle();
    let session_id = agent.session_id;
    let (input_tx, mut input_rx) = mpsc::channel(16);
    let (output_tx, output_rx) = mpsc::channel(512);
    let start_mode = agent.mode();
    let resumed = restored.is_some();
    let (replay, history) = restored
        .map(|r| (r.events, r.history))
        .unwrap_or_else(|| (Vec::new(), Vec::new()));
    let tui = tokio::spawn(latch_tui::run(
        input_tx,
        output_rx,
        start_mode,
        info.profile.model.clone(),
        replay,
        history,
    ));
    send_profile_header(
        &context,
        &output_tx,
        &workspace,
        resumed,
        &info.profile,
        &info.descriptor,
    )
    .await?;
    // Provider-neutral catalogs for the TUI selectors. The TUI never sees base
    // URLs, model families, or wire parameters.
    output_tx
        .send(Output::InferenceCatalog(context.catalog()))
        .await?;
    output_tx
        .send(Output::SetupCatalog(context.setup_catalog()))
        .await?;
    if info.needs_setup {
        output_tx.send(Output::SetupRequired).await?;
    }
    // Policy chrome state, restored from durable events on resume.
    output_tx.send(Output::Safety(agent.safety())).await?;
    output_tx
        .send(Output::Permissions(agent.permissions()))
        .await?;
    // CLI `--attach` images were ingested through the same kernel path as
    // `/attach`; they become pending TUI attachments for the first prompt.
    for media in &initial_attachments {
        output_tx.send(Output::Attachment(media.clone())).await?;
    }
    let mut outcome = InteractiveOutcome::Exit;
    // A steer that races the end of a run is handed back by the kernel. It is
    // surfaced as the next ordinary request instead of lingering in a queue
    // that no run will consume.
    let mut carry: Option<(String, Vec<MediaRef>)> = None;
    'session: loop {
        let input = match carry.take() {
            Some((text, media)) => Input::Submit { text, media },
            None => match input_rx.recv().await {
                Some(input) => input,
                None => break,
            },
        };
        match input {
            Input::Quit => break,
            Input::Resume => {
                outcome = InteractiveOutcome::Resume;
                break;
            }
            Input::Cancel => {}
            Input::Permission {
                request_id,
                approved,
            } => {
                broker.resolve(request_id, approved).await;
            }
            Input::SetSafety(safety) => {
                agent.set_safety(safety)?;
                output_tx.send(Output::Safety(safety)).await?;
            }
            Input::SetPermissions(mode) => {
                agent.set_permissions(mode)?;
                output_tx.send(Output::Permissions(mode)).await?;
            }
            Input::SetInferenceProfile {
                provider,
                model,
                effort,
            } => {
                apply_live_profile(
                    &mut agent,
                    &context,
                    InferenceProfile::new(provider, model, effort),
                    &output_tx,
                    &workspace,
                    resumed,
                )
                .await?;
            }
            Input::SetupApply(plan) => {
                apply_setup(
                    &mut agent,
                    &mut context,
                    plan,
                    &output_tx,
                    &workspace,
                    resumed,
                )
                .await?;
            }
            Input::Attach(path) => {
                match ingest_attachments(&context.config, session_id, &[PathBuf::from(path)]) {
                    Ok(mut media) => {
                        if let Some(media) = media.pop() {
                            output_tx.send(Output::Attachment(media)).await?;
                        }
                    }
                    Err(error) => {
                        output_tx
                            .send(Output::Notice(format!("error: {error:#}")))
                            .await?;
                    }
                }
            }
            Input::Submit { text, media } => {
                if is_slash_command_input(&text) {
                    handle_command(&mut agent, &text, &output_tx).await?;
                    continue;
                }
                let active = CancellationToken::new();
                let tx = output_tx.clone();
                let sink = Arc::new(move |event: AgentOutput| {
                    let outputs: Vec<Output> = match event {
                        AgentOutput::Transient(StreamEvent::TextDelta(t)) => {
                            vec![Output::AssistantDelta(t)]
                        }
                        AgentOutput::Durable(e) => {
                            vec![Output::Event(e)]
                        }
                        AgentOutput::ToolResult(result) => {
                            vec![Output::ToolResult(result)]
                        }
                        _ => vec![],
                    };
                    for output in outputs {
                        let _ = tx.try_send(output);
                    }
                });
                let running = agent.run(UserInput::new(text, media), active.clone(), sink);
                tokio::pin!(running);
                loop {
                    tokio::select! {
                        result = &mut running => {
                            match result {
                                Ok(_) => output_tx.send(Output::AssistantDone).await?,
                                Err(error) => output_tx.send(Output::Notice(format!("error: {error:#}"))).await?,
                            }
                            break;
                        }
                        next = input_rx.recv() => match next {
                            Some(Input::Cancel) => active.cancel(),
                            Some(Input::Permission { request_id, approved }) => { broker.resolve(request_id, approved).await; }
                            Some(Input::Quit) | None => { active.cancel(); let _ = (&mut running).await; break 'session; }
                            Some(Input::Resume) => { output_tx.send(Output::Notice("cancel the active turn before resuming another session".into())).await?; }
                            Some(Input::SetSafety(_)) | Some(Input::SetPermissions(_)) | Some(Input::SetInferenceProfile { .. }) | Some(Input::SetupApply(_)) => {
                                output_tx.send(Output::Notice("finish or cancel the active turn before changing the inference profile".into())).await?;
                            }
                            Some(Input::Attach(path)) => {
                                match ingest_attachments(&context.config, session_id, &[PathBuf::from(path)]) {
                                    Ok(mut media) => {
                                        if let Some(media) = media.pop() {
                                            output_tx.send(Output::Attachment(media)).await?;
                                        }
                                    }
                                    Err(error) => {
                                        output_tx.send(Output::Notice(format!("error: {error:#}"))).await?;
                                    }
                                }
                            }
                            Some(Input::Submit { text, media }) => {
                                if is_slash_command_input(&text) {
                                    output_tx.send(Output::Notice("finish or cancel the active turn before running commands".into())).await?;
                                } else {
                                    // Live steering: the kernel accepts it for
                                    // the current run or rejects it once the
                                    // run has closed. A rejected message is
                                    // immediately re-sent as a new request so
                                    // no user input is silently dropped.
                                    match steering.push(UserInput::new(text.clone(), media.clone())) {
                                        SteeringSubmission::Accepted => {
                                            output_tx.send(Output::Notice("steering queued".into())).await?;
                                        }
                                        SteeringSubmission::Closed => {
                                            output_tx.send(Output::Notice("run finished; sending as a new request".into())).await?;
                                            carry = Some((text, media));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    drop(output_tx);
    tui.await??;
    agent.shutdown_extensions().await?;
    Ok(outcome)
}

/// Resolves and applies a live profile change, then refreshes the chrome.
async fn apply_live_profile(
    agent: &mut Agent,
    context: &InferenceContext,
    requested: InferenceProfile,
    tx: &mpsc::Sender<Output>,
    workspace: &Path,
    resumed: bool,
) -> Result<()> {
    let (profile, descriptor) = context.resolve(&requested)?;
    let provider = match context.build(&profile, &descriptor, agent.session_id) {
        Ok(provider) => provider,
        Err(error) => {
            tx.send(Output::Notice(format!("error: {error:#}"))).await?;
            return Ok(());
        }
    };
    agent.set_inference_profile(
        provider,
        profile.clone(),
        &descriptor,
        context.context.clone(),
        "selected with /model",
    )?;
    send_profile_header(context, tx, workspace, resumed, &profile, &descriptor).await
}

/// Persists a `/setup` apply plan to configuration and, for a directly
/// entered secret, to the 0600 local credential store. Returns the resolved
/// (provider id, model, effort). Secret values never reach config.toml.
fn persist_setup(
    context: &mut InferenceContext,
    plan: &SetupPlan,
) -> Result<(String, String, ReasoningEffort)> {
    let SetupPlan::Apply {
        name,
        provider_kind,
        base_url,
        credential,
        model,
        effort,
    } = plan.clone()
    else {
        bail!("persist_setup only handles apply plans");
    };
    let kind = ProviderKind::parse(&provider_kind, base_url.as_deref())
        .ok_or_else(|| anyhow!("unknown provider kind {provider_kind:?}"))?;
    // Provider id is instance identity. The setup flow defaults it to the kind
    // id but users may name multiple instances of one kind.
    let provider_id = if name.trim().is_empty() {
        kind.id().to_owned()
    } else {
        name.trim().to_owned()
    };
    if model.trim().is_empty() {
        bail!("provider {provider_id:?} needs a default_model before saving");
    }
    let credential_ref = match credential {
        SetupCredential::Env(name) => CredentialRef::Env(name),
        SetupCredential::Secret(secret) => {
            context.credentials.set(&provider_id, &secret)?;
            CredentialRef::File(provider_id.clone())
        }
    };
    // Semantic update: an existing entry keeps its model metadata, display
    // name, and discovery flag; only the fields the flow owns are replaced.
    let entry = context
        .config
        .providers
        .entry(provider_id.clone())
        .or_default();
    if entry.kind != kind {
        // A kind change invalidates kind-specific built-in model overrides.
        entry.models.clear();
    }
    entry.kind = kind;
    entry.base_url = base_url;
    entry.credential = Some(credential_ref.display());
    entry.default_model = Some(model.clone());
    let valid_inference = context
        .config
        .inference
        .provider
        .as_deref()
        .zip(context.config.inference.model.as_deref())
        .is_some_and(|(provider, model)| {
            ProviderRegistry::from_config(&context.config)
                .ok()
                .and_then(|registry| {
                    registry
                        .resolve_profile(&InferenceProfile::new(
                            provider,
                            model,
                            context.config.inference.effort,
                        ))
                        .ok()
                })
                .is_some()
        });
    if !valid_inference {
        context.config.inference = InferenceConfig {
            provider: Some(provider_id.clone()),
            model: Some(model.clone()),
            effort,
        };
    }
    save_config(context)?;
    Ok((provider_id, model, effort))
}

/// Removes a provider instance from configuration. Returns an inference
/// profile when the removed provider was the active one and a replacement was
/// selected; the caller must then switch the live agent. Credential material
/// (environment variables and the local secrets file) is never deleted.
fn remove_provider(context: &mut InferenceContext, name: &str) -> Result<Option<InferenceProfile>> {
    if !context.config.providers.contains_key(name) {
        bail!("no provider named {name:?}");
    }
    let active = context.config.inference.provider.as_deref() == Some(name);
    if active {
        bail!(
            "provider {name:?} is the new-session default; set another provider as the new-session default before removing it"
        );
    }
    context.config.providers.remove(name);
    let replacement = None;
    save_config(context)?;
    Ok(replacement)
}

fn save_config(context: &InferenceContext) -> Result<()> {
    if let Some(path) = context.config_path.clone().or_else(Config::default_path) {
        context.config.save(&path)?;
    }
    Ok(())
}

/// Persists a `/setup` plan and applies the resulting change live.
async fn apply_setup(
    agent: &mut Agent,
    context: &mut InferenceContext,
    plan: SetupPlan,
    tx: &mpsc::Sender<Output>,
    workspace: &Path,
    resumed: bool,
) -> Result<()> {
    if let SetupPlan::Remove { name } = &plan {
        let replacement = remove_provider(context, name)?;
        // Rebuild the registry so the removal is reflected everywhere.
        context.registry = ProviderRegistry::from_config(&context.config)?;
        agent.set_provider_factory(context.provider_factory());
        match replacement {
            Some(requested) => {
                let (profile, descriptor) = context.resolve(&requested)?;
                let provider = context.build(&profile, &descriptor, agent.session_id)?;
                agent.set_inference_profile(
                    provider,
                    profile.clone(),
                    &descriptor,
                    context.context.clone(),
                    "active provider removed",
                )?;
                tx.send(Output::Notice(format!(
                    "removed provider {name}; active profile moved to {} · {} ({})",
                    context.provider_label(profile.provider.as_str()),
                    profile.model,
                    profile.effort.label()
                )))
                .await?;
                return send_profile_header(context, tx, workspace, resumed, &profile, &descriptor)
                    .await;
            }
            None => {
                tx.send(Output::Notice(format!(
                    "removed provider {name}; {} remain",
                    context.config.providers.len()
                )))
                .await?;
                return Ok(());
            }
        }
    }
    let (provider_id, model, effort) = persist_setup(context, &plan)?;
    // Rebuild the registry from the persisted configuration so resolution
    // matches what the next process will load.
    context.registry = ProviderRegistry::from_config(&context.config)?;
    agent.set_provider_factory(context.provider_factory());
    let requested = InferenceProfile::new(provider_id, model, effort);
    let (profile, descriptor) = context.resolve(&requested)?;
    let provider = context.build(&profile, &descriptor, agent.session_id)?;
    agent.set_inference_profile(
        provider,
        profile.clone(),
        &descriptor,
        context.context.clone(),
        "configured with /setup",
    )?;
    tx.send(Output::Notice(format!(
        "configured {} · {} ({}) — saved",
        context.provider_label(profile.provider.as_str()),
        profile.model,
        profile.effort.label()
    )))
    .await?;
    send_profile_header(context, tx, workspace, resumed, &profile, &descriptor).await
}

async fn send_profile_header(
    context: &InferenceContext,
    tx: &mpsc::Sender<Output>,
    workspace: &Path,
    resumed: bool,
    profile: &InferenceProfile,
    descriptor: &ModelDescriptor,
) -> Result<()> {
    tx.send(Output::Header {
        model: profile.model.clone(),
        provider: context.provider_label(profile.provider.as_str()),
        provider_id: profile.provider.to_string(),
        effort: profile.effort,
        workspace: workspace.display().to_string(),
        branch: git_branch().unwrap_or_else(|_| "-".into()),
        resumed,
        pricing: descriptor.pricing.clone(),
    })
    .await?;
    Ok(())
}

fn is_slash_command_input(text: &str) -> bool {
    !text.contains(['\n', '\r']) && text.trim_start().starts_with('/')
}

async fn handle_command(agent: &mut Agent, text: &str, tx: &mpsc::Sender<Output>) -> Result<()> {
    let mut parts = text.split_whitespace();
    match parts.next().unwrap_or("") {
        "/mode" => {
            if let Some(value) = parts.next() {
                let mode = Mode::from_str(value).map_err(anyhow::Error::msg)?;
                agent.set_mode(mode)?;
                tx.send(Output::Mode(mode)).await?;
            } else {
                tx.send(Output::Notice(format!("mode: {}", agent.mode())))
                    .await?;
            }
        }
        "/safety" => {
            if let Some(value) = parts.next() {
                let safety = latch_protocol::Safety::from_str(value).map_err(anyhow::Error::msg)?;
                agent.set_safety(safety)?;
                tx.send(Output::Safety(safety)).await?;
            } else {
                tx.send(Output::Notice(format!(
                    "safety: {} (use /safety to select, or /safety strict|standard|autonomous)",
                    agent.safety()
                )))
                .await?;
            }
        }
        "/permissions" => {
            if let Some(value) = parts.next() {
                let mode =
                    latch_protocol::PermissionMode::from_str(value).map_err(anyhow::Error::msg)?;
                agent.set_permissions(mode)?;
                tx.send(Output::Permissions(mode)).await?;
            } else {
                tx.send(Output::Notice(format!(
                    "permissions: {} (use /permissions to select, or /permissions auto|human|ai)",
                    agent.permissions()
                )))
                .await?;
            }
        }
        "/context" => {
            let c = agent.context(None)?;
            let s = &c.stats;
            tx.send(Output::Notice(format!(
                "context ≈{} / {} tok ({}; estimated) | system {} | state {} | conversation {} | images {} ({} tok) | reasoning {} | tool-args {} | tool-results {} | recall {} | tools {} | ext {} | reserve {} | headroom {} | {} events | {}/{} episodes",
                s.total_tokens, s.window_tokens, s.status, s.instructions_tokens, s.state_tokens,
                s.conversation_tokens, s.image_count, s.image_tokens, s.reasoning_replay_tokens,
                s.tool_arguments_tokens, s.tool_result_tokens, s.recall_tokens, s.tools_tokens,
                s.extension_tokens, s.reserve_tokens, s.headroom_tokens, s.durable_events,
                s.selected_episodes, s.episodes
            ))).await?;
        }
        "/compact" => {
            agent.compact()?;
            tx.send(Output::Notice(
                "active context reset; durable history and state retained".into(),
            ))
            .await?;
        }
        "/diff" => send_tool(agent, "git_diff", tx).await?,
        "/group" => {
            tx.send(Output::Notice(agent.group_overview_text())).await?;
        }
        "/checkpoint" => send_tool(agent, "checkpoint", tx).await?,
        "/undo" => send_tool(agent, "undo", tx).await?,
        "/model" => {
            let profile = agent.profile();
            tx.send(Output::Notice(format!(
                "inference profile: {} · {} ({}) — use /model to change",
                profile.provider,
                profile.model,
                profile.effort.label()
            )))
            .await?;
        }
        "/help" => {
            let commands = SLASH_COMMANDS
                .iter()
                .map(|c| format!("{}  {}", c.name, c.description))
                .collect::<Vec<_>>()
                .join("\n");
            tx.send(Output::Notice(format!(
                "modes: /mode ask|plan|work (WORK mutates; ASK/PLAN are read-only)\n\
                 safety: /safety strict|standard|autonomous (ASK/PLAN remain read-only)\n\
                 permissions: /permissions auto|human|ai (how an Ask is resolved)\n\
                 composer: Enter send · Ctrl+J or Alt+Enter newline · Home/End line · Ctrl+Home/End buffer\n\
                 composer scroll: PgUp/PgDn or mouse wheel when the prompt overflows\n\
                 transcript: Shift+PgUp/PgDn · Shift+Home/End · mouse wheel\n\
                 input: Ctrl+A/E line start/end · Ctrl+W delete word · Ctrl+U/K delete to line edges\n\
                 history: Up/Down at the first/last composer line recalls previous prompts\n\
                 interrupt: Ctrl+C cancels a running turn, or quits when idle\n\
                 palette: typing / filters commands · ↑/↓ select · Tab complete · Enter run · Esc close\n\
                 detail: Ctrl+T or /raw · sidebar: Ctrl+B or /sidebar · resume: /resume\n\
                 diff: /diff opens the inspector (↑/↓ PgUp/PgDn · Ctrl+T raw · Esc close)\n\
                 permission: when a tool needs approval, y approves and n/Esc denies\n\
                 commands:\n{commands}"
            ))).await?;
        }
        other => {
            tx.send(Output::Notice(format!(
                "unknown command {other}; use /help"
            )))
            .await?
        }
    }
    Ok(())
}
async fn send_tool(agent: &mut Agent, name: &str, tx: &mpsc::Sender<Output>) -> Result<()> {
    let event_tx = tx.clone();
    let sink: latch_kernel::AgentEventSink = Arc::new(move |event| {
        if let latch_kernel::agent::AgentOutput::Durable(event) = event {
            let _ = event_tx.try_send(Output::Event(event));
        }
    });
    let r = agent
        .builtin_tool_streamed(name, CancellationToken::new(), &sink)
        .await;
    if name == "git_diff" {
        // The transcript keeps a compact semantic diff cell; the inspector
        // opens full-width with its own scrolling and raw toggle.
        tx.send(Output::ToolResult(r.clone())).await?;
        if !r.output.trim().is_empty() {
            tx.send(Output::Diff(r.output)).await?;
        }
        return Ok(());
    }
    tx.send(Output::ToolResult(r)).await?;
    Ok(())
}
fn git_branch() -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .output()?;
    let mut branch = String::from_utf8(out.stdout)?.trim().to_string();
    let dirty = !std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()?
        .stdout
        .is_empty();
    if dirty {
        branch.push('*');
    }
    Ok(branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use latch_kernel::CredentialStore;

    #[test]
    fn help_lists_every_palette_command() {
        // Single source of truth: /help and the palette agree.
        let help_text = SLASH_COMMANDS
            .iter()
            .map(|c| c.name)
            .collect::<Vec<_>>()
            .join(" ");
        for required in [
            "/mode",
            "/model",
            "/context",
            "/group",
            "/diff",
            "/checkpoint",
            "/undo",
            "/compact",
            "/resume",
            "/raw",
            "/help",
            "/quit",
            "/exit",
        ] {
            assert!(help_text.contains(required), "/help missing {required}");
        }
    }

    #[tokio::test]
    async fn mode_override_via_cli_flag_restores_session_mode() {
        // exercises the precedence helper used by build_agent indirectly; the
        // full precedence matrix is covered in latch_kernel::session tests.
        let mode = latch_kernel::session::resumed_mode(&[], Some(Mode::Ask), Mode::Work);
        assert_eq!(mode, Mode::Ask);
    }

    #[test]
    fn explicit_resume_flags_are_deterministic() {
        let args = Args::try_parse_from(["latch", "--resume", "--session", "deadbeef"]).unwrap();
        assert_eq!(args.session.as_deref(), Some("deadbeef"));
        assert!(!args.latest);
        assert!(Args::try_parse_from(["latch", "--session", "deadbeef"]).is_err());
        assert!(
            Args::try_parse_from(["latch", "--resume", "--latest", "--session", "deadbeef"])
                .is_err()
        );
    }

    #[test]
    fn only_single_line_slash_input_is_a_control_command() {
        assert!(is_slash_command_input("/help"));
        assert!(is_slash_command_input("  /mode plan"));
        assert!(!is_slash_command_input("/help\nthis is prompt content"));
        assert!(!is_slash_command_input("plain prompt"));
    }

    #[test]
    fn setup_secret_reaches_only_the_private_store_and_never_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_dir = dir.path().join("state");
        let config = Config {
            state_dir: state_dir.clone(),
            ..Config::default()
        };
        let mut context = InferenceContext::new(config, Some(config_path.clone())).unwrap();
        let plan = SetupPlan::Apply {
            name: "deepseek".into(),
            provider_kind: "deepseek".into(),
            base_url: Some("https://api.deepseek.com".into()),
            credential: SetupCredential::Secret("sk-super-secret".into()),
            model: "deepseek-v4.1-flash".into(),
            effort: ReasoningEffort::High,
        };
        let (provider_id, model, effort) = persist_setup(&mut context, &plan).unwrap();
        assert_eq!(provider_id, "deepseek");
        assert_eq!(model, "deepseek-v4.1-flash");
        assert_eq!(effort, ReasoningEffort::High);
        let config_text = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !config_text.contains("sk-super-secret"),
            "a secret must never be written to config: {config_text}"
        );
        assert!(config_text.contains("credential = \"file:deepseek\""));
        assert!(config_text.contains("[inference]"));
        let secrets_path = CredentialStore::default_path(&state_dir);
        let store = CredentialStore::open(&secrets_path).unwrap();
        assert_eq!(
            store
                .require(&CredentialRef::File("deepseek".into()))
                .unwrap(),
            "sk-super-secret"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&secrets_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    fn setup_plan(
        name: &str,
        kind: &str,
        credential: SetupCredential,
        model: &str,
        effort: ReasoningEffort,
    ) -> SetupPlan {
        SetupPlan::Apply {
            name: name.into(),
            provider_kind: kind.into(),
            base_url: None,
            credential,
            model: model.into(),
            effort,
        }
    }

    #[test]
    fn setup_preserves_model_metadata_and_allows_multiple_instances() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_dir = dir.path().join("state");
        let config = Config {
            state_dir: state_dir.clone(),
            ..Config::default()
        };
        let mut context = InferenceContext::new(config, Some(config_path.clone())).unwrap();
        persist_setup(
            &mut context,
            &setup_plan(
                "openai-main",
                "openai",
                SetupCredential::Secret("sk-1".into()),
                "gpt-5.5",
                ReasoningEffort::Medium,
            ),
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(&config_path)
                .unwrap()
                .contains("[providers.openai-main]")
        );

        // Simulate user metadata added to the provider table by hand.
        context
            .config
            .providers
            .get_mut("openai-main")
            .unwrap()
            .models
            .insert(
                "gpt-5.5".into(),
                latch_kernel::config::ModelConfig {
                    context_window_tokens: Some(4_242),
                    ..Default::default()
                },
            );

        // Editing the same instance updates credential/effort but keeps the
        // model metadata it did not own.
        persist_setup(
            &mut context,
            &setup_plan(
                "openai-main",
                "openai",
                SetupCredential::Env("OPENAI_API_KEY".into()),
                "gpt-5.5",
                ReasoningEffort::High,
            ),
        )
        .unwrap();
        let entry = context.config.providers.get("openai-main").unwrap();
        assert_eq!(
            entry
                .models
                .get("gpt-5.5")
                .and_then(|model| model.context_window_tokens),
            Some(4_242)
        );
        assert_eq!(entry.credential.as_deref(), Some("env:OPENAI_API_KEY"));

        // A second instance of the same kind coexists with a stable id.
        persist_setup(
            &mut context,
            &setup_plan(
                "openai-proxy",
                "openai",
                SetupCredential::Env("PROXY_KEY".into()),
                "gpt-5.5",
                ReasoningEffort::Low,
            ),
        )
        .unwrap();
        assert_eq!(context.config.providers.len(), 2);
        assert!(context.config.providers.contains_key("openai-proxy"));
    }

    #[test]
    fn provider_removal_keeps_inference_valid_and_preserves_other_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_dir = dir.path().join("state");
        let config = Config {
            state_dir: state_dir.clone(),
            ..Config::default()
        };
        let mut context = InferenceContext::new(config, Some(config_path.clone())).unwrap();
        for (name, kind, model) in [
            ("openai-main", "openai", "gpt-5.5"),
            ("deepseek", "deepseek", "deepseek-flash"),
        ] {
            persist_setup(
                &mut context,
                &setup_plan(
                    name,
                    kind,
                    SetupCredential::Env("KEY".into()),
                    model,
                    ReasoningEffort::ProviderDefault,
                ),
            )
            .unwrap();
        }
        // The first usable provider seeds the new-session default. Later
        // provider saves keep it unchanged.
        assert_eq!(
            context.config.inference.provider.as_deref(),
            Some("openai-main")
        );
        // Metadata on the default instance survives removal of a sibling.
        context
            .config
            .providers
            .get_mut("openai-main")
            .unwrap()
            .models
            .insert(
                "gpt-5.5".into(),
                latch_kernel::config::ModelConfig {
                    context_window_tokens: Some(555),
                    ..Default::default()
                },
            );
        let replacement = remove_provider(&mut context, "deepseek").unwrap();
        assert!(replacement.is_none(), "non-active removal needs no switch");
        assert_eq!(
            context.config.inference.provider.as_deref(),
            Some("openai-main")
        );
        assert!(remove_provider(&mut context, "missing").is_err());

        // Adding and removing another provider does not rewrite [inference].
        persist_setup(
            &mut context,
            &setup_plan(
                "lab",
                "openai",
                SetupCredential::Env("LAB_KEY".into()),
                "gpt-5.5",
                ReasoningEffort::ProviderDefault,
            ),
        )
        .unwrap();
        assert_eq!(
            context.config.inference.provider.as_deref(),
            Some("openai-main")
        );
        let replacement = remove_provider(&mut context, "lab").unwrap();
        assert!(replacement.is_none());
        assert_eq!(
            context.config.inference.provider.as_deref(),
            Some("openai-main")
        );

        // The persisted config reloads, resolves a default profile, and the
        // surviving instance keeps its metadata.
        let reloaded = Config::load(Some(&config_path)).unwrap();
        let registry = ProviderRegistry::from_config(&reloaded).unwrap();
        let (profile, _) = registry.default_profile(&reloaded).unwrap();
        assert_eq!(profile.provider.as_str(), "openai-main");
        assert_eq!(
            reloaded.providers["openai-main"].models["gpt-5.5"].context_window_tokens,
            Some(555)
        );

        // Removing the default provider requires an explicit default change.
        let error = remove_provider(&mut context, "openai-main").unwrap_err();
        assert!(error.to_string().contains("new-session default"));
        assert!(context.config.providers.contains_key("openai-main"));
    }

    #[test]
    fn cli_profile_flags_parse_effort() {
        let args = Args::try_parse_from([
            "latch",
            "--model",
            "deepseek-v4.1-flash",
            "--effort",
            "high",
        ])
        .unwrap();
        assert_eq!(args.model.as_deref(), Some("deepseek-v4.1-flash"));
        assert_eq!(
            args.effort
                .as_deref()
                .unwrap()
                .parse::<ReasoningEffort>()
                .unwrap(),
            ReasoningEffort::High
        );
        assert!(
            Args::try_parse_from(["latch", "--effort", "ultra"]).is_ok(),
            "parsing is validated when building the profile, not by clap"
        );
    }
}
