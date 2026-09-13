#![forbid(unsafe_code)]

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use latch_kernel::{
    Agent, AgentRuntime, Config, ContinuityEngine, CredentialRef, CredentialStore, EventStore,
    ModelDescriptor, ModelProvider, PolicyEngine, ProviderRegistry, ToolExecutor,
    agent::{AgentOutput, SteeringSubmission},
    config::{InferenceConfig, ProviderKind, ProviderProfileConfig},
    prompt::PromptCompiler,
    provider::StreamSink,
    session,
};
use latch_protocol::{
    EventPayload, InferenceProfile, Mode, ProviderId, ReasoningEffort, StreamEvent,
};
use latch_tui::{
    CatalogModel, CatalogProvider, InferenceCatalog, Input, Output, SLASH_COMMANDS, SetupCredential,
    SetupKind, SetupPlan,
};
use std::io::IsTerminal;
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
    /// Resume an exact session UUID or unambiguous UUID prefix.
    #[arg(long, requires = "resume")]
    session: Option<String>,
    /// Resume the most recently active session in this workspace.
    #[arg(long, requires = "resume", conflicts_with = "session")]
    latest: bool,
    #[arg(long,value_parser=parse_mode)]
    mode: Option<Mode>,
    /// Override the configured provider for this invocation.
    #[arg(long)]
    provider: Option<String>,
    /// Override the configured model for this invocation.
    #[arg(long)]
    model: Option<String>,
    /// Override the reasoning effort for this invocation.
    #[arg(long)]
    effort: Option<String>,
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
    let mut workspace = std::env::current_dir()?;
    if let Some(Commands::Debug {
        command: DebugCommand::Prompt { fragment, mode },
    }) = args.command
    {
        return debug_prompt(&workspace, mode, fragment.as_deref());
    }
    let mut selected = match select_startup_session(&workspace, &config, &args).await? {
        ResumeChoice::Session(id, session_workspace) => {
            workspace = session_workspace;
            Some(id)
        }
        ResumeChoice::Fresh => None,
        ResumeChoice::Exit => return Ok(()),
    };
    let interactive = args.prompt.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    let mut built = build_agent(&workspace, &config, &args, selected, interactive).await?;
    if let Some(prompt) = args.prompt {
        return one_shot(&mut built.agent, &prompt).await;
    }
    loop {
        match interactive_session(
            built.agent,
            built.info,
            built.context,
            built.restored,
            workspace.clone(),
        )
        .await?
        {
            InteractiveOutcome::Exit => return Ok(()),
            InteractiveOutcome::Resume => {
                selected = match pick_session(&workspace, &config).await? {
                    ResumeChoice::Session(id, session_workspace) => {
                        workspace = session_workspace;
                        Some(id)
                    }
                    ResumeChoice::Fresh => None,
                    ResumeChoice::Exit => return Ok(()),
                };
                built = build_agent(&workspace, &config, &args, selected, true).await?;
            }
        }
    }
}

/// Everything needed to resolve and switch inference profiles at runtime.
struct InferenceContext {
    registry: ProviderRegistry,
    credentials: CredentialStore,
    context: latch_kernel::config::ContextConfig,
    config: Config,
    config_path: Option<PathBuf>,
}

impl InferenceContext {
    fn new(config: Config, config_path: Option<PathBuf>) -> Result<Self> {
        let registry = ProviderRegistry::from_config(&config)?;
        let credentials = CredentialStore::open(CredentialStore::default_path(&config.state_dir))?;
        let context = config.context.clone();
        Ok(Self {
            registry,
            credentials,
            context,
            config,
            config_path,
        })
    }

    fn resolve(&self, requested: &InferenceProfile) -> Result<(InferenceProfile, ModelDescriptor)> {
        self.registry.resolve_profile(requested)
    }

    fn build(
        &self,
        profile: &InferenceProfile,
        descriptor: &ModelDescriptor,
        session_id: Uuid,
    ) -> Result<Arc<dyn ModelProvider>> {
        self.registry
            .build_provider(profile, descriptor, &self.credentials, session_id)
    }

    fn provider_label(&self, id: &str) -> String {
        self.registry
            .provider(id)
            .map(|profile| profile.display_name.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    /// Provider-neutral catalog for the live `/model` selector.
    fn catalog(&self) -> InferenceCatalog {
        InferenceCatalog {
            providers: self
                .registry
                .available_providers()
                .into_iter()
                .map(|provider| CatalogProvider {
                    id: provider.id.to_string(),
                    display_name: provider.display_name.clone(),
                    models: provider
                        .available_models()
                        .into_iter()
                        .map(catalog_model)
                        .collect(),
                })
                .collect(),
        }
    }

    /// Provider kinds and built-in models available to `/setup`.
    fn setup_catalog(&self) -> Vec<SetupKind> {
        [
            ProviderKind::OpenCodeGo,
            ProviderKind::DeepSeek,
            ProviderKind::OpenAi,
            ProviderKind::Anthropic,
            ProviderKind::OpenAiCompatible,
        ]
        .into_iter()
        .map(|kind| SetupKind {
            kind: kind.id().to_owned(),
            label: kind.display_name().to_owned(),
            default_base_url: kind.default_base_url().to_owned(),
            credential_label: kind.default_credential().to_owned(),
            default_model: latch_kernel::providers::builtin_catalog(kind)
                .first()
                .map(|descriptor| descriptor.model.clone())
                .unwrap_or_default(),
            models: latch_kernel::providers::builtin_catalog(kind)
                .iter()
                .map(catalog_model_ref)
                .collect(),
        })
        .collect()
    }
}

fn catalog_model(descriptor: &ModelDescriptor) -> CatalogModel {
    catalog_model_ref(descriptor)
}

fn catalog_model_ref(descriptor: &ModelDescriptor) -> CatalogModel {
    CatalogModel {
        id: descriptor.model.clone(),
        display_name: descriptor.display_name.clone(),
        efforts: descriptor.supported_efforts.clone(),
        default_effort: descriptor.default_effort,
    }
}

/// One resolved session plus everything the interactive layer needs.
struct SessionInfo {
    profile: InferenceProfile,
    descriptor: ModelDescriptor,
}

struct BuiltSession {
    agent: Agent,
    info: SessionInfo,
    context: InferenceContext,
    restored: Option<Restored>,
}

enum ResumeChoice {
    Session(Uuid, PathBuf),
    Fresh,
    Exit,
}

async fn select_startup_session(
    workspace: &Path,
    config: &Config,
    args: &Args,
) -> Result<ResumeChoice> {
    if !args.resume {
        return Ok(ResumeChoice::Fresh);
    }
    let store = EventStore::open(&config.state_dir.join("latch.sqlite3"))?;
    if let Some(selector) = &args.session {
        let selected = store.resolve_session(selector)?;
        if Path::new(&selected.workspace) != workspace {
            eprintln!(
                "resuming session {} from workspace {} (current workspace is {})",
                &selected.id.to_string()[..8],
                selected.workspace,
                workspace.display()
            );
        }
        return Ok(ResumeChoice::Session(
            selected.id,
            selected.workspace.into(),
        ));
    }
    if args.latest {
        return store
            .latest_session(Some(workspace))?
            .map(|id| ResumeChoice::Session(id, workspace.to_path_buf()))
            .ok_or_else(|| {
                anyhow!(
                    "no previous session for {}; remove --latest to start fresh",
                    workspace.display()
                )
            });
    }
    let matching = store.list_sessions(Some(workspace))?;
    match matching.as_slice() {
        [] => Err(anyhow!("no previous session for {}", workspace.display())),
        [session] => Ok(ResumeChoice::Session(
            session.id,
            session.workspace.clone().into(),
        )),
        _ if args.prompt.is_some()
            || !std::io::stdin().is_terminal()
            || !std::io::stdout().is_terminal() =>
        {
            bail!(
                "{} sessions match {}; choose one with --resume --session <uuid-or-prefix> or use --resume --latest",
                matching.len(),
                workspace.display()
            )
        }
        _ => pick_session(workspace, config).await,
    }
}

async fn pick_session(workspace: &Path, config: &Config) -> Result<ResumeChoice> {
    let store = EventStore::open(&config.state_dir.join("latch.sqlite3"))?;
    let sessions = store
        .list_sessions(None)?
        .into_iter()
        .map(|session| latch_tui::SessionItem {
            id: session.id,
            workspace: session.workspace,
            updated_at: session.updated_at,
            mode: session
                .mode
                .map_or_else(|| config.default_mode.to_string(), |mode| mode.to_string()),
            model: session.model.unwrap_or_else(|| "—".into()),
            prompt: session
                .prompt_preview
                .unwrap_or_else(|| "No user prompt".into()),
            event_count: session.event_count,
        })
        .collect();
    let preview_store = store.clone();
    let preview = Arc::new(move |id| {
        preview_store
            .session_preview(id, 6)
            .unwrap_or_default()
            .into_iter()
            .map(|line| latch_tui::SessionPreviewLine {
                speaker: line.speaker.into(),
                text: line.text,
            })
            .collect()
    });
    Ok(
        match latch_tui::run_session_picker(sessions, workspace, preview).await? {
            latch_tui::PickerSelection::Resume(id) => {
                let selected = store.resolve_session(&id.to_string())?;
                ResumeChoice::Session(id, selected.workspace.into())
            }
            latch_tui::PickerSelection::StartFresh => ResumeChoice::Fresh,
            latch_tui::PickerSelection::Exit | latch_tui::PickerSelection::Cancel => {
                ResumeChoice::Exit
            }
        },
    )
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
            "fragments: {}  estimated tokens: ≈{}\n",
            p.fragments.len(),
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
        println!("\n{}", p.text);
    }
    Ok(())
}

/// One restored session for the TUI: visible transcript items and prompt
/// history, both derived from durable events by the shared formatter.
struct Restored {
    events: Vec<latch_protocol::Event>,
    history: Vec<String>,
}

async fn build_agent(
    workspace: &Path,
    config: &Config,
    args: &Args,
    resume_session: Option<Uuid>,
    interactive: bool,
) -> Result<BuiltSession> {
    let db = config.state_dir.join("latch.sqlite3");
    let store = EventStore::open(&db)?;
    let mut restored = None;
    let resume = resume_session.is_some();
    let session_id = if let Some(session) = resume_session {
        // Approval requests that were pending at exit can no longer be
        // answered; mark them durably before the transcript replay so resume
        // shows honest state instead of a phantom prompt.
        let expired = Agent::expire_pending_permissions(&store, session)?;
        if expired > 0 {
            tracing::info!("expired {expired} unresolved permission request(s)");
        }
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
            events: events.clone(),
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
    let mode = session::resumed_mode(&events, args.mode, config.default_mode);

    // Inference profile precedence: explicit CLI override > the session's own
    // durable profile > configured default. Credentials are resolved fresh
    // from the environment/local store at this moment and are never persisted.
    let context = InferenceContext::new(config.clone(), args.config.clone())?;
    let (default_profile, _default_descriptor) = context.registry.default_profile(config)?;
    let resumed_profile = resume
        .then(|| session::resumed_inference_profile(&events))
        .flatten();
    let mut requested = resumed_profile.clone().unwrap_or_default();
    if !resume || resumed_profile.is_none() {
        requested = default_profile.clone();
    }
    let mut overridden = false;
    if let Some(provider) = &args.provider {
        requested.provider = ProviderId::new(provider.clone());
        requested.model.clear();
        requested.effort = ReasoningEffort::ProviderDefault;
        overridden = true;
    }
    if let Some(model) = &args.model {
        requested.model = model.clone();
        overridden = true;
    }
    if let Some(effort) = &args.effort {
        requested.effort = effort
            .parse::<ReasoningEffort>()
            .map_err(anyhow::Error::msg)?;
        overridden = true;
    }
    let (profile, descriptor) = context.resolve(&requested)?;
    let provider = match context.build(&profile, &descriptor, session_id) {
        Ok(provider) => provider,
        Err(error) if interactive => {
            // First-run UX: rather than failing before the TUI can offer
            // /setup, start the session with an actionable stub provider.
            tracing::warn!("{error:#}");
            Arc::new(UnconfiguredProvider {
                message: format!("{error:#}"),
            })
        }
        Err(error) => return Err(error),
    };
    let policy = PolicyEngine::with_defaults(
        mode,
        workspace.to_path_buf(),
        config.permissions.clone(),
        config.safety.level,
    );
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
    let continuity =
        ContinuityEngine::for_model(store.clone(), config.context.clone(), &profile.model);
    let mut agent = Agent::new(AgentRuntime {
        session_id,
        workspace: workspace.to_path_buf(),
        mode,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity,
        retry_budget: config.failure.retry_budget,
    });
    agent.set_stagnation_budget(config.failure.stagnation_budget);
    agent.set_max_model_turns(config.failure.max_model_turns);
    if overridden {
        // A command-line override is durable provenance so a later resume
        // keeps the profile the user actually selected.
        agent.set_inference_profile(
            provider,
            profile.clone(),
            &descriptor,
            config.context.clone(),
            "command line override",
        )?;
    } else {
        agent.restore_inference_profile(
            provider,
            profile.clone(),
            &descriptor,
            config.context.clone(),
        );
    }
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
        // Resume restores the exact policy the session ended in; it never
        // silently broadens or narrows permissions.
        agent.restore_policy(
            session::resumed_safety(&events, config.safety.level),
            session::resumed_permissions(&events, config.permissions.mode),
        );
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
        // not silently forgotten, and progress supervision reconstructs an
        // active inspection loop.
        agent.restore_failures()?;
        agent.restore_progress()?;
    }
    Ok(BuiltSession {
        agent,
        info: SessionInfo {
            profile,
            descriptor,
        },
        context,
        restored,
    })
}

/// Stand-in provider used when no credential is configured and Latch is
/// interactive: the session starts so `/setup` can run, and any attempt to
/// use the model fails with the actionable configuration error.
struct UnconfiguredProvider {
    message: String,
}

#[async_trait]
impl ModelProvider for UnconfiguredProvider {
    fn name(&self) -> &str {
        "unconfigured"
    }
    fn model(&self) -> &str {
        "unconfigured"
    }
    async fn stream(
        &self,
        _request: latch_protocol::ModelRequest,
        _cancel: CancellationToken,
        _sink: StreamSink,
    ) -> Result<latch_protocol::ModelResponse> {
        bail!("{}", self.message)
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
) -> Result<InteractiveOutcome> {
    // The TUI is the only path that can approve `Ask` policy decisions.
    agent.enable_interactive_permissions();
    let broker = agent.permission_broker();
    let steering = agent.steering_handle();
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
    // Policy chrome state, restored from durable events on resume.
    output_tx.send(Output::Safety(agent.safety())).await?;
    output_tx
        .send(Output::Permissions(agent.permissions()))
        .await?;
    let mut outcome = InteractiveOutcome::Exit;
    // A steer that races the end of a run is handed back by the kernel. It is
    // surfaced as the next ordinary request instead of lingering in a queue
    // that no run will consume.
    let mut carry: Option<String> = None;
    'session: loop {
        let input = match carry.take() {
            Some(text) => Input::Submit(text),
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
                    provider,
                    model,
                    effort,
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
            Input::Submit(text) => {
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
                            Some(Input::Permission { request_id, approved }) => { broker.resolve(request_id, approved).await; }
                            Some(Input::Quit) | None => { active.cancel(); let _ = (&mut running).await; break 'session; }
                            Some(Input::Resume) => { output_tx.send(Output::Notice("cancel the active turn before resuming another session".into())).await?; }
                            Some(Input::SetSafety(_)) | Some(Input::SetPermissions(_)) | Some(Input::SetInferenceProfile { .. }) | Some(Input::SetupApply(_)) => {
                                output_tx.send(Output::Notice("finish or cancel the active turn before changing the inference profile".into())).await?;
                            }
                            Some(Input::Submit(text)) => {
                                if is_slash_command_input(&text) {
                                    output_tx.send(Output::Notice("finish or cancel the active turn before running commands".into())).await?;
                                } else {
                                    // Live steering: the kernel accepts it for
                                    // the current run or rejects it once the
                                    // run has closed. A rejected message is
                                    // immediately re-sent as a new request so
                                    // no user input is silently dropped.
                                    match steering.push(text.clone()) {
                                        SteeringSubmission::Accepted => {
                                            output_tx.send(Output::Notice("steering queued".into())).await?;
                                        }
                                        SteeringSubmission::Closed => {
                                            output_tx.send(Output::Notice("run finished; sending as a new request".into())).await?;
                                            carry = Some(text);
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
    provider: String,
    model: String,
    effort: ReasoningEffort,
    tx: &mpsc::Sender<Output>,
    workspace: &Path,
    resumed: bool,
) -> Result<()> {
    let requested = InferenceProfile::new(provider, model, effort);
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

/// Persists a `/setup` plan, resolves the new provider, and switches live.
async fn apply_setup(
    agent: &mut Agent,
    context: &mut InferenceContext,
    plan: SetupPlan,
    tx: &mpsc::Sender<Output>,
    workspace: &Path,
    resumed: bool,
) -> Result<()> {
    let SetupPlan::Apply {
        provider_kind,
        base_url,
        credential,
        model,
        effort,
    } = plan;
    let kind = ProviderKind::parse(&provider_kind, base_url.as_deref())
        .ok_or_else(|| anyhow!("unknown provider kind {provider_kind:?}"))?;
    let provider_id = kind.id().to_owned();
    // Secret values only ever travel TUI -> CLI -> 0600 local store. They are
    // never written to config.toml, the event log, or the transcript.
    let credential_ref = match credential {
        SetupCredential::Env(name) => CredentialRef::Env(name),
        SetupCredential::Secret(secret) => {
            context.credentials.set(&provider_id, &secret)?;
            CredentialRef::File(provider_id.clone())
        }
    };
    context.config.providers.insert(
        provider_id.clone(),
        ProviderProfileConfig {
            kind,
            display_name: None,
            base_url,
            credential: Some(credential_ref.display()),
            default_model: Some(model.clone()),
            models: std::collections::BTreeMap::new(),
            model_discovery: false,
        },
    );
    context.config.inference = InferenceConfig {
        provider: Some(provider_id.clone()),
        model: Some(model.clone()),
        effort,
    };
    if let Some(path) = context.config_path.clone().or_else(Config::default_path) {
        context.config.save(&path)?;
    }
    // Rebuild the registry from the persisted configuration so resolution
    // matches what the next process will load.
    context.registry = ProviderRegistry::from_config(&context.config)?;
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
            } else { tx.send(Output::Notice(format!("mode: {}", agent.mode()))).await?; }
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
                let mode = latch_protocol::PermissionMode::from_str(value).map_err(anyhow::Error::msg)?;
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
                "context ≈{} / {} tok ({}; estimated) | system {} | state {} | conversation {} | reasoning {} | tool-args {} | tool-results {} | recall {} | tools {} | ext {} | reserve {} | headroom {} | {} events | {}/{} episodes",
                s.total_tokens, s.window_tokens, s.status, s.instructions_tokens, s.state_tokens,
                s.conversation_tokens, s.reasoning_replay_tokens, s.tool_arguments_tokens,
                s.tool_result_tokens, s.recall_tokens, s.tools_tokens, s.extension_tokens,
                s.reserve_tokens, s.headroom_tokens, s.durable_events, s.selected_episodes, s.episodes
            ))).await?;
        }
        "/compact" => { agent.compact()?; tx.send(Output::Notice("active context reset; durable history and state retained".into())).await?; }
        "/diff" => send_tool(agent, "git_diff", tx).await?,
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
        }        "/help" => {
            let commands = SLASH_COMMANDS.iter().map(|c| format!("{}  {}", c.name, c.description)).collect::<Vec<_>>().join("\n");
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
        other => tx.send(Output::Notice(format!("unknown command {other}; use /help"))).await?,
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
        let mode = session::resumed_mode(&[], Some(Mode::Ask), Mode::Work);
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
}
