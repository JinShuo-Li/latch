#![forbid(unsafe_code)]

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use latch_kernel::{
    Agent, AgentRuntime, AnthropicProvider, Config, ContinuityEngine, EventStore, ModelProvider,
    OpenAiProvider, PolicyEngine, ToolExecutor, prompt::PromptCompiler, session,
};
use latch_protocol::{EventPayload, Mode, StreamEvent, TaskState};
use latch_tui::{Input, Output, SLASH_COMMANDS};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "latch",
    version,
    about = "A quiet, programmable terminal coding agent"
)]
struct Args {
    /// Continue the latest session for this workspace: visible transcript,
    /// effective mode, task state, evidence, failures, and change ownership.
    #[arg(long)]
    resume: bool,
    #[arg(long,value_parser=parse_mode)]
    mode: Option<Mode>,
    #[arg(short = 'p', long)]
    prompt: Option<String>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Subcommand)]
enum Commands {
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
}
#[derive(Subcommand)]
enum DebugCommand {
    Prompt {
        #[arg(long)]
        fragment: Option<String>,
        #[arg(long,value_parser=parse_mode,default_value="work")]
        mode: Mode,
    },
}
fn parse_mode(s: &str) -> Result<Mode, String> {
    Mode::from_str(s)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let config = Config::load(args.config.as_deref())?;
    let workspace = std::env::current_dir()?;
    if let Some(Commands::Debug {
        command: DebugCommand::Prompt { fragment, mode },
    }) = args.command
    {
        return debug_prompt(&workspace, mode, fragment.as_deref());
    }
    let (mut agent, model, session) =
        build_agent(&workspace, &config, args.mode, args.resume).await?;
    if let Some(prompt) = args.prompt {
        return one_shot(&mut agent, &prompt).await;
    }
    interactive(agent, model, session).await
}

fn debug_prompt(workspace: &Path, mode: Mode, id: Option<&str>) -> Result<()> {
    let p = PromptCompiler::compile(mode, &TaskState::default(), workspace)?;
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
            "fragments: {}  approximate tokens: {}\n",
            p.fragments.len(),
            p.approximate_tokens()
        );
        for f in &p.fragments {
            println!(
                "{:>4}  {:<38} v{}  ~{} tokens",
                f.priority,
                f.id,
                f.version,
                f.content.len().div_ceil(4)
            );
        }
        println!("\n{}", p.text);
    }
    Ok(())
}

/// One restored session for the TUI: visible transcript items and prompt
/// history, both derived from durable events by the shared formatter.
struct Restored {
    items: Vec<latch_protocol::DisplayItem>,
    history: Vec<String>,
}

async fn build_agent(
    workspace: &Path,
    config: &Config,
    cli_mode: Option<Mode>,
    resume: bool,
) -> Result<(Agent, String, Option<Restored>)> {
    let db = config.state_dir.join("latch.sqlite3");
    let store = EventStore::open(&db)?;
    let mut restored = None;
    let session_id = if resume {
        let session = store
            .latest_session(Some(workspace))?
            .ok_or_else(|| anyhow!("no previous session for {}", workspace.display()))?;
        let events = store.events(session)?;
        store.append(session, EventPayload::SessionResumed)?;
        for (id, description) in store.interrupted_operations(session)? {
            store.append(
                session,
                EventPayload::OperationInterrupted {
                    operation_id: id,
                    description,
                },
            )?;
            store.mark_operation_reported(id)?;
        }
        restored = Some(Restored {
            items: session::replay_items(&events),
            history: session::prompt_history(&events),
        });
        session
    } else {
        let session = store.create_session(workspace)?;
        let (head, dirty_paths) = observe_git(workspace);
        store.append(
            session,
            EventPayload::GitStateObserved { head, dirty_paths },
        )?;
        session
    };
    let events = store.events(session_id)?;
    // Mode precedence (both fresh and resumed): explicit CLI --mode > the
    // session's durable mode history > configured default.
    let mode = session::resumed_mode(&events, cli_mode, config.default_mode);
    let provider = provider(config, session_id)?;
    let model = provider.model().to_string();
    let policy = PolicyEngine::new(mode, workspace.to_path_buf(), config.permissions.clone());
    let artifacts = config
        .state_dir
        .join("artifacts")
        .join(session_id.to_string());
    let tools = ToolExecutor::new(
        workspace.to_path_buf(),
        artifacts.clone(),
        store.clone(),
        session_id,
        policy,
    )?;
    if resume {
        // Restore durable change ownership before anything can mutate.
        let count = tools.restore_ownership().await?;
        tracing::info!("restored {count} owned change records");
    }
    let continuity = ContinuityEngine::new(store.clone(), config.context.clone());
    let mut agent = Agent::new(AgentRuntime {
        session_id,
        workspace: workspace.to_path_buf(),
        mode,
        store: store.clone(),
        provider,
        tools,
        continuity,
        retry_budget: config.failure.retry_budget,
    });
    for extension in config
        .extensions
        .iter()
        .filter(|extension| extension.enabled)
    {
        agent
            .load_extension(extension.name.clone(), &extension.command, &extension.args)
            .await
            .with_context(|| format!("initialize extension {}", extension.name))?;
    }
    if resume {
        if let Some(state) = events.iter().rev().find_map(|e| match &e.payload {
            EventPayload::TaskStateUpdated { state } => Some(state.clone()),
            _ => None,
        }) {
            agent.restore_state(state);
        }
        agent.restore_evidence(
            events
                .iter()
                .filter_map(|event| match &event.payload {
                    EventPayload::EvidenceCreated { evidence } => Some(evidence.clone()),
                    _ => None,
                })
                .collect(),
        );
        // Failure supervision reconstructs its streaks so a stalled loop is
        // not silently forgotten.
        agent.restore_failures()?;
    }
    Ok((agent, model, restored))
}

fn provider(config: &Config, session_id: Uuid) -> Result<Arc<dyn ModelProvider>> {
    let p = &config.provider;
    match p.kind.as_str() {
        "openai" | "openai-compatible" => {
            let env = p.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");
            let key = std::env::var(env)
                .with_context(|| format!("set {env} or configure provider.api_key_env"))?;
            Ok(Arc::new(
                OpenAiProvider::new(
                    p.base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.openai.com/v1".into()),
                    key,
                    p.model.clone(),
                )
                .with_session(session_id),
            ))
        }
        "anthropic" => {
            let env = p.api_key_env.as_deref().unwrap_or("ANTHROPIC_API_KEY");
            let key = std::env::var(env)
                .with_context(|| format!("set {env} or configure provider.api_key_env"))?;
            Ok(Arc::new(AnthropicProvider::new(
                p.base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.anthropic.com".into()),
                key,
                p.model.clone(),
            )))
        }
        other => bail!("unsupported provider {other}; expected openai-compatible or anthropic"),
    }
}

async fn one_shot(agent: &mut Agent, prompt: &str) -> Result<()> {
    let sink = Arc::new(|event: latch_kernel::agent::AgentOutput| {
        if let latch_kernel::agent::AgentOutput::Transient(StreamEvent::TextDelta(text)) = event {
            print!("{text}");
        }
    });
    agent.run(prompt, CancellationToken::new(), sink).await?;
    println!();
    agent.shutdown_extensions().await?;
    Ok(())
}

async fn interactive(mut agent: Agent, model: String, restored: Option<Restored>) -> Result<()> {
    let (input_tx, mut input_rx) = mpsc::channel(16);
    let (output_tx, output_rx) = mpsc::channel(512);
    let start_mode = agent.mode();
    let (replay, history) = restored
        .map(|r| (r.items, r.history))
        .unwrap_or_else(|| (Vec::new(), Vec::new()));
    let tui = tokio::spawn(latch_tui::run(
        input_tx,
        output_rx,
        start_mode,
        model.clone(),
        replay,
        history,
    ));
    output_tx
        .send(Output::Header {
            model,
            branch: git_branch().unwrap_or_else(|_| "-".into()),
            continuity: "bounded".into(),
        })
        .await?;
    'session: while let Some(input) = input_rx.recv().await {
        match input {
            Input::Quit => break,
            Input::Cancel => {}
            Input::Submit(text) => {
                if text.trim_start().starts_with('/') {
                    handle_command(&mut agent, &text, &output_tx).await?;
                    continue;
                }
                let active = CancellationToken::new();
                let tx = output_tx.clone();
                let sink = Arc::new(move |event: latch_kernel::agent::AgentOutput| {
                    let outputs: Vec<Output> = match event {
                        latch_kernel::agent::AgentOutput::Transient(StreamEvent::TextDelta(t)) => {
                            vec![Output::AssistantDelta(t)]
                        }
                        latch_kernel::agent::AgentOutput::Durable(e) => {
                            // The streamed assistant item already shows the
                            // final text; skip the durable duplicate.
                            if matches!(e.payload, EventPayload::AssistantMessageCompleted { .. }) {
                                vec![]
                            } else {
                                latch_protocol::display_items(&e)
                                    .into_iter()
                                    .map(Output::Item)
                                    .collect()
                            }
                        }
                        latch_kernel::agent::AgentOutput::ToolResult(result) => {
                            vec![Output::Item(latch_protocol::DisplayItem::ToolActivity {
                                call_id: result.call_id,
                                verb: result.name,
                                target: String::new(),
                                detail: first_line(&result.output),
                                status: if result.is_error {
                                    latch_protocol::ToolRunStatus::Failed
                                } else {
                                    latch_protocol::ToolRunStatus::Passed
                                },
                            })]
                        }
                        _ => vec![],
                    };
                    for output in outputs {
                        let _ = tx.try_send(output);
                    }
                });
                let running = agent.run(&text, active.clone(), sink);
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
                            Some(Input::Quit) | None => { active.cancel(); let _ = (&mut running).await; break 'session; }
                            Some(Input::Submit(_)) => output_tx.send(Output::Notice("finish or cancel the active turn before submitting another message".into())).await?,
                        }
                    }
                }
            }
        }
    }
    drop(output_tx);
    tui.await??;
    agent.shutdown_extensions().await?;
    Ok(())
}

async fn handle_command(agent: &mut Agent, text: &str, tx: &mpsc::Sender<Output>) -> Result<()> {
    let mut parts = text.split_whitespace();
    match parts.next().unwrap_or("") {
        "/mode" => {
            if let Some(value) = parts.next() {
                let mode = Mode::from_str(value).map_err(anyhow::Error::msg)?;
                agent.set_mode(mode)?;
                tx.send(Output::Mode(mode)).await?;
            } else { tx.send(Output::Notice(format!("mode: {}", agent.mode()))).await?; }
        }
        "/context" => {
            let c = agent.context(None)?;
            tx.send(Output::Notice(format!(
                "ctx {} B (status {}) | recent {} B | recalled {} B | canonical {} B | reserve {} B | {} events | {}/{} episodes",
                c.stats.total_bytes, c.stats.status, c.stats.recent_bytes, c.stats.recalled_bytes, c.stats.canonical_bytes, c.stats.reserve_bytes, c.stats.durable_events, c.stats.selected_episodes, c.stats.episodes
            ))).await?;
        }
        "/compact" => { agent.compact()?; tx.send(Output::Notice("active context reset; durable history and state retained".into())).await?; }
        "/diff" => send_tool(agent, "git_diff", tx).await?,
        "/checkpoint" => send_tool(agent, "checkpoint", tx).await?,
        "/undo" => send_tool(agent, "undo", tx).await?,
        "/model" => tx.send(Output::Notice("model changes require config and a new invocation in V0.1; durable sessions remain provider-independent".into())).await?,
        "/help" => {
            let commands = SLASH_COMMANDS.iter().map(|c| format!("{}  {}", c.name, c.description)).collect::<Vec<_>>().join("\n");
            tx.send(Output::Notice(format!(
                "modes: /mode ask|plan|work (WORK mutates; ASK/PLAN are read-only)\n\
                 scroll: PgUp/PgDn, Home/End, mouse wheel — Ctrl+C cancels a running turn\n\
                 input: Enter submit · Alt+Enter newline · Ctrl+A/E line start/end · Ctrl+W delete word\n\
                 history: Up/Down recalls previous prompts\n\
                 palette: typing / filters commands · Tab/Enter complete · Esc close\n\
                 commands:\n{commands}"
            ))).await?;
        }
        other => tx.send(Output::Notice(format!("unknown command {other}; use /help"))).await?,
    }
    Ok(())
}
async fn send_tool(agent: &Agent, name: &str, tx: &mpsc::Sender<Output>) -> Result<()> {
    let r = agent.builtin_tool(name, CancellationToken::new()).await;
    tx.send(Output::Item(latch_protocol::DisplayItem::ToolActivity {
        call_id: r.call_id,
        verb: name.into(),
        target: String::new(),
        detail: first_line(&r.output),
        status: if r.is_error {
            latch_protocol::ToolRunStatus::Failed
        } else {
            latch_protocol::ToolRunStatus::Passed
        },
    }))
    .await?;
    Ok(())
}
fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect()
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

fn observe_git(workspace: &Path) -> (Option<String>, Vec<String>) {
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned());
    let dirty_paths = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.get(3..).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (head, dirty_paths)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "/diff",
            "/checkpoint",
            "/undo",
            "/compact",
            "/help",
            "/quit",
        ] {
            assert!(help_text.contains(required), "/help missing {required}");
        }
    }

    #[tokio::test]
    async fn mode_override_via_cli_flag_restores_session_mode() {
        // exercises the precedence helper used by build_agent indirectly; the
        // full precedence matrix is covered in latch_kernel::session tests.
        let mode = session::resumed_mode(&[], Some(Mode::Ask), Mode::Work);
        assert_eq!(mode, Mode::Ask);
    }
}
