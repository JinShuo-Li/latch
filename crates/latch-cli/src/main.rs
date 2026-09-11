#![forbid(unsafe_code)]

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use latch_kernel::{
    Agent, AgentRuntime, AnthropicProvider, Config, ContinuityEngine, EventStore, ModelProvider,
    OpenAiProvider, PolicyEngine, ToolExecutor, prompt::PromptCompiler,
};
use latch_protocol::{EventPayload, Mode, StreamEvent, TaskState};
use latch_tui::{Input, Output, ToolStatus};
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
    let mode = args.mode.unwrap_or(config.default_mode);
    let (mut agent, model) = build_agent(&workspace, &config, mode, args.resume).await?;
    if let Some(prompt) = args.prompt {
        return one_shot(&mut agent, &prompt).await;
    }
    interactive(agent, mode, model).await
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

async fn build_agent(
    workspace: &Path,
    config: &Config,
    mode: Mode,
    resume: bool,
) -> Result<(Agent, String)> {
    let db = config.state_dir.join("latch.sqlite3");
    let store = EventStore::open(&db)?;
    let session_id = if resume {
        store
            .latest_session(Some(workspace))?
            .ok_or_else(|| anyhow!("no previous session for {}", workspace.display()))?
    } else {
        let session = store.create_session(workspace)?;
        let (head, dirty_paths) = observe_git(workspace);
        store.append(
            session,
            EventPayload::GitStateObserved { head, dirty_paths },
        )?;
        session
    };
    if resume {
        store.append(session_id, EventPayload::SessionResumed)?;
        for (id, description) in store.interrupted_operations(session_id)? {
            store.append(
                session_id,
                EventPayload::OperationInterrupted {
                    operation_id: id,
                    description,
                },
            )?;
            store.mark_operation_reported(id)?;
        }
    }
    let provider = provider(config, session_id)?;
    let model = provider.model().to_string();
    let policy = PolicyEngine::new(mode, workspace.to_path_buf(), config.permissions.clone());
    let tools = ToolExecutor::new(
        workspace.to_path_buf(),
        config
            .state_dir
            .join("artifacts")
            .join(session_id.to_string()),
        store.clone(),
        session_id,
        policy,
    )?;
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
    if resume
        && let Some(state) = store.events(session_id)?.iter().rev().find_map(|e| {
            if let EventPayload::TaskStateUpdated { state } = &e.payload {
                Some(state.clone())
            } else {
                None
            }
        })
    {
        agent.restore_state(state);
    }
    if resume {
        let evidence = store
            .events(session_id)?
            .into_iter()
            .filter_map(|event| match event.payload {
                EventPayload::EvidenceCreated { evidence } => Some(evidence),
                _ => None,
            })
            .collect();
        agent.restore_evidence(evidence);
    }
    Ok((agent, model))
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

async fn interactive(mut agent: Agent, mode: Mode, model: String) -> Result<()> {
    let (input_tx, mut input_rx) = mpsc::channel(16);
    let (output_tx, output_rx) = mpsc::channel(256);
    let tui = tokio::spawn(latch_tui::run(input_tx, output_rx, mode, model.clone()));
    output_tx
        .send(Output::Header {
            model,
            branch: git_branch().unwrap_or_else(|_| "-".into()),
            continuity: "healthy".into(),
        })
        .await?;
    'session: while let Some(input) = input_rx.recv().await {
        match input {
            Input::Quit => break,
            Input::Cancel => {}
            Input::Submit(text) => {
                if text.starts_with('/') {
                    handle_command(&mut agent, &text, &output_tx).await?;
                    continue;
                }
                let active = CancellationToken::new();
                let tx = output_tx.clone();
                let sink = Arc::new(move |event: latch_kernel::agent::AgentOutput| {
                    let output = match event {
                        latch_kernel::agent::AgentOutput::Transient(StreamEvent::TextDelta(t)) => {
                            Some(Output::AssistantDelta(t))
                        }
                        latch_kernel::agent::AgentOutput::Durable(e) => match e.payload {
                            EventPayload::ToolRequested { call } => Some(Output::Tool {
                                verb: call.name,
                                target: short_args(&call.arguments),
                                status: ToolStatus::Running,
                            }),
                            _ => None,
                        },
                        latch_kernel::agent::AgentOutput::ToolResult(result) => {
                            Some(Output::Tool {
                                verb: if result.is_error {
                                    "fail".into()
                                } else {
                                    "done".into()
                                },
                                target: format!(
                                    "{}  {}",
                                    result.name,
                                    result.output.lines().next().unwrap_or("")
                                ),
                                status: if result.is_error {
                                    ToolStatus::Failed
                                } else {
                                    ToolStatus::Passed
                                },
                            })
                        }
                        _ => None,
                    };
                    if let Some(output) = output {
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
            tx.send(Output::Notice(format!("continuity:{} | recent {} B | recalled {} B | canonical {} B | code/evidence {} B | reserve {} B | {} events | {} episodes", c.stats.status, c.stats.recent_bytes, c.stats.recalled_bytes, c.stats.canonical_bytes, c.stats.code_evidence_bytes, c.stats.reserve_bytes, c.stats.durable_events, c.stats.episodes))).await?;
        }
        "/compact" => { agent.compact()?; tx.send(Output::Notice("active context reset; durable history and state retained".into())).await?; }
        "/diff" => send_tool(agent, "git_diff", tx).await?,
        "/checkpoint" => send_tool(agent, "checkpoint", tx).await?,
        "/undo" => send_tool(agent, "undo", tx).await?,
        "/model" => tx.send(Output::Notice("model changes require config and a new invocation in V0.1; durable sessions remain provider-independent".into())).await?,
        "/help" => tx.send(Output::Notice("/mode [ask|plan|work]  /model  /context  /diff  /checkpoint  /undo  /compact  /help  /quit".into())).await?,
        other => tx.send(Output::Notice(format!("unknown command {other}; use /help"))).await?,
    }
    Ok(())
}
async fn send_tool(agent: &Agent, name: &str, tx: &mpsc::Sender<Output>) -> Result<()> {
    let r = agent.builtin_tool(name, CancellationToken::new()).await;
    tx.send(Output::Tool {
        verb: name.into(),
        target: r.output,
        status: if r.is_error {
            ToolStatus::Failed
        } else {
            ToolStatus::Passed
        },
    })
    .await?;
    Ok(())
}
fn short_args(v: &serde_json::Value) -> String {
    v.get("path")
        .or_else(|| v.get("command"))
        .or_else(|| v.get("query"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .chars()
        .take(72)
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
