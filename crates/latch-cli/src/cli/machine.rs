//! Machine-oriented execution: `latch run`, `latch resume`, and the legacy
//! `-p` one-shot compatibility path.
//!
//! Machine mode never opens the TUI and never waits for a human. Permission
//! decisions the configured policy cannot resolve non-interactively are
//! denied by the kernel (unchanged); when any such denial occurs the final
//! structured status is `permission_denied` and the process exits with
//! [`EXIT_PERMISSION`]. There is no bypass flag.

use crate::cli::command::{Args, OutputFormat, PromptArgs, ResumeArgs, RunArgs};
use crate::cli::output::{
    AgentGraphReport, ContextReport, ErrorReport, EventsReport, MachineResult, Reporter, RunStatus,
    TaskReport, UsageAggregate, UsageScopes, ValidationReport, completion_name, exit_code,
};
use crate::cli::session::{
    ProfileOverrides, SelectedSession, SessionBuildError, SessionRequest, build_agent,
    ingest_attachments, resolve_workspace, select_session,
};
use latch_kernel::{AgentEventSink, Config, EventStore, EvidenceLedger, agent::AgentOutput};
use latch_protocol::{
    EventPayload, EvidenceStatus, InferenceProfile, StreamEvent, TaskState, UserInput,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PERMISSION_MESSAGE: &str = "permission denied: a required operation needs interactive approval, which machine mode \
     cannot provide; nothing was auto-approved. Choose a narrower operation or configure an \
     autonomous policy explicitly";

#[derive(Debug, Clone)]
pub enum PromptSource {
    Inline(String),
    File(PathBuf),
    Stdin,
}

#[derive(Debug, Clone)]
pub enum ResumeRequest {
    Session(String),
    Latest,
    NewestInWorkspace,
}

#[derive(Debug, Clone)]
pub struct MachineRequest {
    pub command: &'static str,
    pub output: OutputFormat,
    pub config_path: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
    pub prompt: Option<PromptSource>,
    pub attachments: Vec<PathBuf>,
    pub mode: Option<latch_protocol::Mode>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub resume: Option<ResumeRequest>,
}

pub fn run_request(args: &Args, run: RunArgs) -> MachineRequest {
    let mut request = base_request(args, "run");
    request.workspace = run.workspace;
    request.prompt = prompt_source(run.prompt);
    request
}

pub fn resume_request(args: &Args, resume: ResumeArgs) -> MachineRequest {
    let mut request = base_request(args, "resume");
    request.workspace = resume.workspace;
    request.prompt = prompt_source(resume.prompt);
    request.resume = Some(match (&resume.session, resume.latest) {
        (Some(session), _) => ResumeRequest::Session(session.clone()),
        (None, true) => ResumeRequest::Latest,
        (None, false) => ResumeRequest::NewestInWorkspace,
    });
    request
}

/// The legacy `latch -p ...` invocation maps onto the machine run
/// implementation, preserving `--resume`/`--session`/`--latest` selection.
pub fn legacy_request(args: &Args, prompt: String) -> MachineRequest {
    let mut request = base_request(args, "run");
    request.prompt = Some(PromptSource::Inline(prompt));
    request.resume = if args.resume {
        Some(match (&args.session, args.latest) {
            (Some(session), _) => ResumeRequest::Session(session.clone()),
            (None, true) => ResumeRequest::Latest,
            (None, false) => ResumeRequest::NewestInWorkspace,
        })
    } else {
        None
    };
    request
}

fn base_request(args: &Args, command: &'static str) -> MachineRequest {
    MachineRequest {
        command,
        output: args.output,
        config_path: args.config.clone(),
        workspace: None,
        prompt: None,
        attachments: args.attach.clone(),
        mode: args.mode,
        provider: args.provider.clone(),
        model: args.model.clone(),
        effort: args.effort.clone(),
        resume: None,
    }
}

fn prompt_source(prompt: PromptArgs) -> Option<PromptSource> {
    if let Some(text) = prompt.prompt {
        Some(PromptSource::Inline(text))
    } else if let Some(path) = prompt.prompt_file {
        Some(PromptSource::File(path))
    } else if prompt.stdin {
        Some(PromptSource::Stdin)
    } else {
        None
    }
}

enum MachineError {
    /// Invalid CLI input, config, provider/model selection, or selectors:
    /// nothing about the runtime can fix it (exit 2).
    Configuration(String),
    /// The configuration resolved, but initialization or execution failed
    /// (exit 1): durable storage, tool/extension setup, provider startup, or
    /// the kernel run itself.
    Runtime(String),
}

impl MachineError {
    fn configuration(message: impl std::fmt::Display) -> Self {
        Self::Configuration(message.to_string())
    }

    fn runtime(message: impl std::fmt::Display) -> Self {
        Self::Runtime(message.to_string())
    }

    fn from_anyhow(error: anyhow::Error) -> Self {
        Self::Configuration(format!("{error:#}"))
    }

    fn from_build(error: SessionBuildError) -> Self {
        match error {
            SessionBuildError::Configuration(error) => Self::Configuration(format!("{error:#}")),
            SessionBuildError::Runtime(error) => Self::Runtime(format!("{error:#}")),
        }
    }
}

#[derive(Default)]
struct Telemetry {
    context: Option<ContextReport>,
    permission_denied: bool,
}

pub async fn execute(request: MachineRequest) -> ExitCode {
    let reporter = Reporter::new(request.output);
    let mut result = MachineResult::new(request.command, workspace_label(&request));
    if let Err(error) = run(&request, reporter, &mut result).await {
        match error {
            MachineError::Configuration(message) => {
                result.fail(RunStatus::ConfigurationError, message);
            }
            MachineError::Runtime(message) => {
                result.fail(RunStatus::Failed, message);
            }
        }
    }
    if request.output == OutputFormat::Text
        && let Some(error) = &result.error
    {
        eprintln!("error: {}", error.message);
    }
    reporter.finish(&result);
    exit_code(result.status)
}

async fn run(
    request: &MachineRequest,
    reporter: Reporter,
    result: &mut MachineResult,
) -> Result<(), MachineError> {
    let prompt = resolve_prompt(request).await?;
    let requested_workspace = match &request.workspace {
        Some(path) => resolve_workspace(path).map_err(MachineError::from_anyhow)?,
        None => std::env::current_dir()
            .and_then(|path| path.canonicalize())
            .map_err(|error| MachineError::configuration(format!("current directory: {error}")))?,
    };
    result.workspace = requested_workspace.display().to_string();

    let config = Config::load(request.config_path.as_deref()).map_err(MachineError::from_anyhow)?;
    let store = EventStore::open(
        &latch_kernel::paths::ResolvedPaths::for_state(&config.state_dir).database_path,
    )
    .map_err(|error| MachineError::Runtime(format!("{error:#}")))?;

    let mut workspace = requested_workspace;
    let mut resume_session = None;
    if let Some(resume) = &request.resume {
        let selection = SessionRequest {
            resume: true,
            session: match resume {
                ResumeRequest::Session(selector) => Some(selector.clone()),
                _ => None,
            },
            latest: matches!(resume, ResumeRequest::Latest),
            non_interactive: true,
        };
        match select_session(&workspace, &config, &selection)
            .await
            .map_err(MachineError::from_anyhow)?
        {
            SelectedSession::Session(id, session_workspace) => {
                resume_session = Some(id);
                workspace = session_workspace;
            }
            SelectedSession::Fresh => {}
            SelectedSession::Exit => return Ok(()),
        }
        result.workspace = workspace.display().to_string();
    }

    let overrides = ProfileOverrides {
        mode: request.mode,
        provider: request.provider.clone(),
        model: request.model.clone(),
        effort: request.effort.clone(),
        config_path: request.config_path.clone(),
    };
    // Cancellation is installed before any extension starts: a hung extension
    // must be interruptible before the agent run loop exists.
    let cancel = CancellationToken::new();
    spawn_cancellation_signals(cancel.clone());
    let mut built = match build_agent(
        &workspace,
        &config,
        &overrides,
        resume_session,
        false,
        &cancel,
    )
    .await
    {
        Ok(built) => built,
        Err(error) if cancel.is_cancelled() => {
            result.fail(
                RunStatus::Cancelled,
                format!("startup cancelled: {error:#}"),
            );
            return Ok(());
        }
        Err(error) => return Err(MachineError::from_build(error)),
    };
    let session_id = built.agent.session_id;
    result.session_id = Some(session_id);
    result.profile = Some(profile_report(&built.info.profile));

    let media = ingest_attachments(&config, session_id, &request.attachments)
        .map_err(MachineError::from_anyhow)?;

    let pre_sequence = store
        .last_sequence(session_id)
        .map_err(|error| MachineError::Runtime(format!("{error:#}")))?;
    // Invocation boundary for child usage: snapshot the durable high-water mark
    // of every child session that already exists, so a resumed child's earlier
    // usage is not re-counted. Children spawned during this run are absent here
    // and therefore count from their own beginning (mark 0).
    let child_start: BTreeMap<Uuid, u64> = {
        let mut marks = BTreeMap::new();
        for child in graph_children(&store, session_id).map_err(MachineError::runtime)? {
            marks.insert(
                child,
                store
                    .last_sequence(child)
                    .map_err(|error| MachineError::Runtime(format!("{error:#}")))?,
            );
        }
        marks
    };
    let telemetry = Arc::new(Mutex::new(Telemetry::default()));
    let sink = machine_sink(reporter, Arc::clone(&telemetry));

    let run_result = built
        .agent
        .run(UserInput::new(prompt, media), cancel.clone(), sink)
        .await;
    let post_sequence = store.last_sequence(session_id).unwrap_or(pre_sequence);

    let mut run_error = match &run_result {
        Ok(text) => {
            result.result.text = Some(text.clone());
            None
        }
        Err(error) => Some(format!("{error:#}")),
    };
    if run_result.is_ok()
        && let Err(error) = built.agent.shutdown_extensions().await
    {
        run_error = Some(format!("shutdown extensions: {error:#}"));
    }

    // Durable semantics after the run. `status` describes this invocation;
    // `task` is the kernel's canonical state, which can still be in progress.
    result.task = Some(task_report(built.agent.state(), built.agent.evidence()));

    // Usage describes this invocation, not the session's lifetime: durable
    // `ModelUsage` events are counted only after the per-session sequence mark
    // captured when the invocation began. Root marks precede the run; child
    // marks were snapshotted above (new children start at 0). Resumed sessions
    // therefore do not re-report historical usage.
    let root_usage =
        durable_usage_after(&store, session_id, pre_sequence).map_err(MachineError::runtime)?;
    let mut graph_usage = root_usage.clone();
    let children = graph_children(&store, session_id).map_err(MachineError::runtime)?;
    let mut graph_events = store
        .event_count(session_id)
        .map_err(|error| MachineError::runtime(format!("{error:#}")))?
        as u64;
    for child in &children {
        let after = child_start.get(child).copied().unwrap_or(0);
        graph_usage
            .merge(&durable_usage_after(&store, *child, after).map_err(MachineError::runtime)?);
        graph_events += store
            .event_count(*child)
            .map_err(|error| MachineError::runtime(format!("{error:#}")))?
            as u64;
    }
    result.usage = UsageScopes {
        scope: "invocation_graph",
        root: root_usage.report(),
        graph: graph_usage.report(),
    };
    result.agent_graph = Some(AgentGraphReport {
        sessions: children.len() as u64 + 1,
        child_sessions: children.len() as u64,
        events: graph_events,
    });

    {
        let telemetry = telemetry.lock().expect("telemetry mutex poisoned");
        if let Some(context) = &telemetry.context {
            result.context = ContextReport {
                request_tokens: context.request_tokens,
                common_prefix_tokens: context.common_prefix_tokens,
                cache_epoch: context.cache_epoch,
            };
        }
        result.events = EventsReport::range(pre_sequence, post_sequence);
        if telemetry.permission_denied {
            result.status = RunStatus::PermissionDenied;
        }
    }

    if request.output == OutputFormat::Text && run_result.is_ok() {
        println!();
    }

    if cancel.is_cancelled() {
        result.status = RunStatus::Cancelled;
        result.error = Some(ErrorReport {
            message: run_error.unwrap_or_else(|| "run cancelled".to_owned()),
        });
    } else if let Some(message) = run_error {
        result.status = RunStatus::Failed;
        result.error = Some(ErrorReport { message });
    } else if result.status == RunStatus::PermissionDenied {
        result.error = Some(ErrorReport {
            message: PERMISSION_MESSAGE.to_owned(),
        });
    } else {
        result.status = RunStatus::Completed;
    }
    Ok(())
}

async fn resolve_prompt(request: &MachineRequest) -> Result<String, MachineError> {
    let source = request.prompt.as_ref().ok_or_else(|| {
        MachineError::configuration(
            "no prompt source: pass --prompt, --prompt-file, or --stdin".to_owned(),
        )
    })?;
    let text = match source {
        PromptSource::Inline(text) => text.clone(),
        PromptSource::File(path) => std::fs::read_to_string(path).map_err(|error| {
            MachineError::configuration(format!("read {}: {error}", path.display()))
        })?,
        PromptSource::Stdin => {
            let mut buffer = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
                .map_err(|error| MachineError::configuration(format!("read stdin: {error}")))?;
            buffer
        }
    };
    let text = text.trim_end_matches(['\n', '\r']).to_owned();
    if text.trim().is_empty() {
        return Err(MachineError::configuration("prompt is empty"));
    }
    Ok(text)
}

/// One session's durable usage for this invocation: `ModelUsage` events strictly
/// after `after_sequence`, summed exactly as they report it. Absent categories
/// stay `None`.
fn durable_usage_after(
    store: &EventStore,
    session: Uuid,
    after_sequence: u64,
) -> anyhow::Result<UsageAggregate> {
    let mut aggregate = UsageAggregate::default();
    for event in store.events_after(session, after_sequence)? {
        if let EventPayload::ModelUsage { usage } = &event.payload {
            aggregate.observe(usage);
        }
    }
    Ok(aggregate)
}

/// Durable child sessions of a root, derived from the persisted spawn graph
/// (`AgentSpawned` identity), deduplicated. The kernel's maximum depth is 1,
/// and every spawn records the root, so this covers the whole graph.
fn graph_children(store: &EventStore, root: Uuid) -> anyhow::Result<Vec<Uuid>> {
    let mut children = BTreeSet::new();
    for event in store.agent_events(root)? {
        if let EventPayload::AgentSpawned { identity, .. } = &event.payload
            && identity.root_session_id == root
        {
            children.insert(identity.agent_id);
        }
    }
    Ok(children.into_iter().collect())
}

fn task_report(state: &TaskState, evidence: &EvidenceLedger) -> TaskReport {
    let mut validation = ValidationReport::default();
    for requirement in &state.required_validations {
        match evidence.status_of(requirement) {
            Some(EvidenceStatus::Passed) => validation.passed += 1,
            Some(EvidenceStatus::Failed) => validation.failed += 1,
            _ => validation.pending += 1,
        }
    }
    TaskReport {
        completion: completion_name(&state.completion),
        goal: (!state.goal.trim().is_empty()).then(|| state.goal.clone()),
        evidence_count: evidence.entries().len() as u64,
        validation,
    }
}

fn machine_sink(reporter: Reporter, telemetry: Arc<Mutex<Telemetry>>) -> AgentEventSink {
    Arc::new(move |event| match event {
        AgentOutput::Transient(StreamEvent::TextDelta(text)) => reporter.text_delta(&text),
        AgentOutput::Transient(_) => {}
        AgentOutput::ToolResult(result) => reporter.tool_result(&result),
        AgentOutput::Durable(event) => {
            match &event.payload {
                EventPayload::ContextMaterialized { stats } => {
                    telemetry.lock().expect("telemetry mutex poisoned").context =
                        Some(ContextReport::from_stats(stats));
                }
                EventPayload::PermissionResolved {
                    approved: false,
                    source,
                    ..
                } if source == "non_interactive" => {
                    telemetry
                        .lock()
                        .expect("telemetry mutex poisoned")
                        .permission_denied = true;
                }
                _ => {}
            }
            reporter.durable_event(&event);
        }
    })
}

/// Maps the standard stop signals onto run cancellation so a machine run ends
/// with an orderly `cancelled` result (exit 4) instead of vanishing: Ctrl+C
/// and, on Unix, SIGTERM (what Docker and CI runners send first).
fn spawn_cancellation_signals(cancel: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut terminate) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = terminate.recv() => {}
                    }
                }
                Err(error) => {
                    tracing::warn!("could not install SIGTERM handler: {error}");
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        cancel.cancel();
    });
}

fn profile_report(profile: &InferenceProfile) -> crate::cli::output::ProfileReport {
    crate::cli::output::ProfileReport {
        provider: profile.provider.to_string(),
        model: profile.model.clone(),
        effort: crate::cli::output::reasoning_effort_name(profile.effort),
    }
}

fn workspace_label(request: &MachineRequest) -> String {
    request
        .workspace
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_default()
}
