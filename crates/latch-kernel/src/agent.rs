use crate::agents::{
    AgentSupervisor, ChildMailbox, GroupCoordinator, ProviderFactory, WorkerSettings,
};
use crate::capability::{
    CapabilityDescriptor, CapabilityId, CapabilityKind, CapabilityLifetime, CapabilityOwner,
    CapabilityRegistry, CapabilityScope,
};
use crate::config::{ContextConfig, DEFAULT_CONTEXT_WINDOW_TOKENS};
use crate::context::{ContextEngine, ContextEngineFactory, ContextEngineSpec, ContextRequest};
use crate::continuity::{ContinuityEngine, continuity_context_engine_factory};
use crate::extension::{ExtensionGuardDecision, ExtensionRegistry};
use crate::permissions::PermissionBroker;
use crate::progress::{DEFAULT_STAGNATION_BUDGET, ProgressSupervisor, StagnationDecision};
use crate::prompt::PromptCompiler;
use crate::provider::{ModelProvider, StreamSink};
use crate::providers::ModelDescriptor;
use crate::safety::{Classification, Decision as SafetyDecision};
use crate::state::{
    EvidenceLedger, FailureManager, StateUpdate, TaskStateManager, failure_subject,
};
use crate::store::EventStore;
use crate::tokens::TokenEstimator;
use crate::tools::{CapabilityGrant, ToolExecutor};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use latch_protocol::{
    AgentEvidenceRef, AgentIdentity, AgentReport, AgentStatus, CompletionState, Event,
    EventPayload, EvidenceStatus, InferenceProfile, InputModality, MemoryKind, MemoryRecord, Mode,
    ModelMessage, ModelRequest, PermissionMode, ReasoningEffort, Safety, StreamEvent, ToolCall,
    ToolDefinition, ToolResult, UserInput, Validity,
};
use request::{common_prefix_bytes, context_messages, request_signature};
use serde_json::json;
use std::any::TypeId;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod agent_controls;
mod dispatch;
mod group_tools;
mod kernel_tools;
mod permissions;
pub(crate) mod request;
mod steering;
mod supervision;
mod validation;

pub use steering::{SteeringQueue, SteeringSubmission};

pub type AgentEventSink = Arc<dyn Fn(AgentOutput) + Send + Sync>;

#[derive(Debug, Clone)]
pub enum AgentOutput {
    Durable(Box<Event>),
    Transient(StreamEvent),
    ToolResult(ToolResult),
}

pub struct Agent {
    pub session_id: Uuid,
    workspace: PathBuf,
    mode: Mode,
    store: EventStore,
    provider: Arc<dyn ModelProvider>,
    tools: ToolExecutor,
    /// Context-engine port. The concrete engine (today
    /// [`ContinuityEngine`]) is selected by the caller at construction; the
    /// loop itself never depends on the implementation.
    continuity: Box<dyn ContextEngine>,
    state: TaskStateManager,
    evidence: EvidenceLedger,
    extensions: ExtensionRegistry,
    failures: FailureManager,
    progress: ProgressSupervisor,
    /// Sequence cursor of the last event already consumed by the progress
    /// supervisor; per-turn supervision only reads strictly newer events.
    progress_watermark: u64,
    /// Sequence cursor of the last event already forwarded to the live sink.
    forward_watermark: AtomicU64,
    /// Call ids whose terminal result was produced by the kernel rather than
    /// by model failure (progress suppression or steering supersession).
    /// Failure supervision must not count them against the model.
    kernel_resolved_calls: HashSet<String>,
    max_model_retries: u32,
    max_model_turns: Option<u32>,
    context_window_tokens: usize,
    estimator: TokenEstimator,
    /// Effective inference selection: configured provider identity, model, and
    /// reasoning effort. Durable changes are appended as
    /// `InferenceProfileChanged`; credentials never live here.
    profile: InferenceProfile,
    /// Provider-neutral input modalities the effective model accepts. Text is
    /// implicit; image input is explicit and checked before any request, so an
    /// image is never silently dropped for a text-only model.
    input_modalities: Vec<InputModality>,
    /// Set when the current turn executed `complete` and the kernel-derived
    /// completion is terminal. Lets the loop exit in the same turn instead of
    /// spending another provider request on a summary already produced.
    terminal_complete: bool,
    permissions: PermissionBroker,
    /// Messages queued while the loop is running; drained only at safe model
    /// boundaries.
    steering: SteeringQueue,
    /// Parent-to-child messages, drained only at safe model boundaries.
    child_mailbox: ChildMailbox,
    /// Canonical serialization of the previous provider-facing request, used to
    /// measure the exact reusable common prefix.
    last_request_signature: Option<String>,
    interactive_permissions: bool,
    last_completion: Option<CompletionState>,
    /// Present only on the root agent. Child workers are independently owned
    /// by this supervisor and cannot control or recursively spawn agents.
    supervisor: Option<AgentSupervisor>,
    /// Root-scoped coordination overlay, present on the root and on every
    /// child so participants can claim tasks and exchange durable messages.
    /// The group never owns workers: execution stays with the supervisor.
    group: Option<GroupCoordinator>,
    agent_depth: u8,
}
/// Construction inputs for one agent. The context engine is a type parameter
/// with [`ContinuityEngine`] as the default, so existing call sites keep
/// constructing the default engine while a caller that provides a different
/// [`ContextEngine`] is accepted unchanged at this boundary.
pub struct AgentRuntime<C: ContextEngine = ContinuityEngine> {
    pub session_id: Uuid,
    pub workspace: PathBuf,
    pub mode: Mode,
    pub store: EventStore,
    pub provider: Arc<dyn ModelProvider>,
    pub tools: ToolExecutor,
    pub continuity: C,
    pub retry_budget: u32,
}
/// The initial child-context policy for a root agent.
///
/// The kernel's own [`ContinuityEngine`] gets the exact continuity default the
/// supervisor used before the factory existed, so ordinary construction stays
/// simple. Any other engine type must state its child policy explicitly with
/// [`Agent::set_context_engine_factory`]; until then children fail closed
/// instead of silently running a different engine than the root.
fn initial_context_engine_factory<C: ContextEngine + 'static>(
    runtime: &AgentRuntime<C>,
) -> ContextEngineFactory {
    if TypeId::of::<C>() == TypeId::of::<ContinuityEngine>() {
        return continuity_context_engine_factory(runtime.store.clone());
    }
    let engine = runtime.continuity.name().to_owned();
    Arc::new(move |_spec: &ContextEngineSpec<'_>| {
        bail!(
            "root context engine `{engine}` defines no child-session policy; \
             install one with Agent::set_context_engine_factory"
        )
    })
}

impl Agent {
    #[must_use]
    pub fn new<C: ContextEngine + 'static>(runtime: AgentRuntime<C>) -> Self {
        let settings = WorkerSettings {
            context: runtime.continuity.config().clone(),
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            retry_budget: runtime.retry_budget,
            stagnation_budget: DEFAULT_STAGNATION_BUDGET,
            max_model_turns: None,
            profile: InferenceProfile::new(
                runtime.provider.name().to_owned(),
                runtime.provider.model().to_owned(),
                ReasoningEffort::ProviderDefault,
            ),
        };
        let context_factory = initial_context_engine_factory(&runtime);
        let supervisor = AgentSupervisor::new(
            runtime.session_id,
            runtime.workspace.clone(),
            runtime.store.clone(),
            runtime.provider.clone(),
            runtime.tools.clone(),
            settings,
            context_factory,
        )
        .expect("reconstruct durable agent graph");
        Self::new_inner(runtime, Some(supervisor), 0)
    }

    pub(crate) fn new_child<C: ContextEngine + 'static>(
        runtime: AgentRuntime<C>,
        depth: u8,
    ) -> Self {
        Self::new_inner(runtime, None, depth)
    }

    fn new_inner<C: ContextEngine + 'static>(
        runtime: AgentRuntime<C>,
        supervisor: Option<AgentSupervisor>,
        agent_depth: u8,
    ) -> Self {
        let estimator = TokenEstimator::for_model(runtime.provider.model());
        let mut continuity: Box<dyn ContextEngine> = Box::new(runtime.continuity);
        // The request estimator and the context budget must agree on the
        // provider model.
        continuity.set_estimator(estimator);
        let profile = InferenceProfile::new(
            runtime.provider.name().to_owned(),
            runtime.provider.model().to_owned(),
            ReasoningEffort::ProviderDefault,
        );
        let progress =
            ProgressSupervisor::new(DEFAULT_STAGNATION_BUDGET, runtime.workspace.clone());
        let group = supervisor.as_ref().map(AgentSupervisor::group);
        Self {
            session_id: runtime.session_id,
            workspace: runtime.workspace,
            mode: runtime.mode,
            store: runtime.store,
            provider: runtime.provider,
            tools: runtime.tools,
            continuity,
            state: TaskStateManager::default(),
            evidence: EvidenceLedger::default(),
            extensions: ExtensionRegistry::new(),
            failures: FailureManager::new(runtime.retry_budget),
            progress,
            progress_watermark: 0,
            forward_watermark: AtomicU64::new(0),
            kernel_resolved_calls: HashSet::new(),
            max_model_retries: 2,
            max_model_turns: None,
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            estimator,
            profile,
            input_modalities: vec![InputModality::Text],
            terminal_complete: false,
            permissions: PermissionBroker::new(),
            steering: SteeringQueue::new(),
            child_mailbox: ChildMailbox::default(),
            last_request_signature: None,
            interactive_permissions: false,
            last_completion: None,
            supervisor,
            group,
            agent_depth,
        }
    }
    /// Enables the interactive human approval path. Non-interactive sessions
    /// (`-p`, kernel APIs, tests) leave this off so an `Ask` resolves as an
    /// explicit, durable non-interactive denial instead of hanging.
    pub fn enable_interactive_permissions(&mut self) {
        self.interactive_permissions = true;
    }
    #[must_use]
    pub fn permission_broker(&self) -> PermissionBroker {
        self.permissions.clone()
    }

    /// Handle for live steering. The interactive layer keeps one while a run
    /// is in flight and pushes user messages into it.
    #[must_use]
    pub fn steering_handle(&self) -> SteeringQueue {
        self.steering.clone()
    }
    #[must_use]
    pub(crate) fn child_mailbox_handle(&self) -> ChildMailbox {
        self.child_mailbox.clone()
    }
    /// Resolves a pending approval. Returns false when the request is unknown
    /// or already resolved, so a stale or forged id can never approve twice.
    pub async fn resolve_permission(&self, request_id: Uuid, approved: bool) -> bool {
        self.permissions.resolve(request_id, approved).await
    }
    /// Configures how many consecutive redundant inspection turns are
    /// tolerated before the kernel re-grounds the model.
    pub fn set_stagnation_budget(&mut self, budget: u32) {
        self.progress.set_budget(budget);
        if let Some(supervisor) = &self.supervisor {
            supervisor.update_settings(|settings| settings.stagnation_budget = budget);
        }
    }
    /// Sets the ultimate model-turn circuit breaker. `None` keeps long,
    /// productive tasks unlimited; the stagnation and failure supervisors
    /// remain the primary loop controls.
    pub fn set_max_model_turns(&mut self, max_model_turns: Option<u32>) {
        self.max_model_turns = max_model_turns;
        if let Some(supervisor) = &self.supervisor {
            supervisor.update_settings(|settings| settings.max_model_turns = max_model_turns);
        }
    }
    /// Installs the token-native context configuration and the resolved model
    /// context window.
    pub fn set_context_budget(&mut self, context: ContextConfig, window_tokens: usize) {
        self.continuity.set_config(context.clone());
        self.context_window_tokens = window_tokens.max(1);
        if let Some(supervisor) = &self.supervisor {
            supervisor.update_settings(|settings| {
                settings.context = context;
                settings.context_window_tokens = window_tokens.max(1);
            });
        }
    }

    /// The effective inference selection of this session.
    #[must_use]
    pub fn profile(&self) -> InferenceProfile {
        self.profile.clone()
    }

    /// Records the inherited profile on a child without appending a durable
    /// event: the child's provider was cloned from the root profile and the
    /// root session carries the authoritative durable provenance.
    pub(crate) fn set_inherited_profile(&mut self, profile: InferenceProfile) {
        self.profile = profile;
    }

    /// Installs the factory used to rebuild pinned child sessions after a root
    /// profile switch, a worker restart, or a process resume. It is the
    /// default for future children and never mutates a running child.
    pub fn set_provider_factory(&mut self, factory: ProviderFactory) {
        if let Some(supervisor) = &self.supervisor {
            supervisor.set_provider_factory(Some(factory));
        }
    }

    /// Installs the context-engine policy for every child of this root: future
    /// spawns, workers rebuilt after a restart, and resumed children all
    /// construct through it. [`ContinuityEngine`] roots already default to the
    /// continuity policy; a caller that selects any other root context engine
    /// must install the matching factory, otherwise child spawn fails closed
    /// rather than silently running a different engine than the root.
    /// This applies to the root only; children cannot spawn grandchildren.
    pub fn set_context_engine_factory(&mut self, factory: ContextEngineFactory) {
        if let Some(supervisor) = &self.supervisor {
            supervisor.set_context_engine_factory(factory);
        }
    }

    /// Switches the live provider, model, and reasoning effort without
    /// recreating the session. Task state, evidence, history, workspace
    /// ownership, permissions, continuity, and child sessions are preserved.
    ///
    /// The change is recorded durably as [`EventPayload::InferenceProfileChanged`]
    /// so resume restores the same profile. Credentials are never recorded.
    /// Continuity observes the event and starts a new cache epoch before the
    /// next request, because reasoning replay and wire semantics changed.
    pub fn set_inference_profile(
        &mut self,
        provider: Arc<dyn ModelProvider>,
        profile: InferenceProfile,
        descriptor: &ModelDescriptor,
        context: ContextConfig,
        reason: &str,
    ) -> Result<()> {
        self.apply_inference_profile(provider, profile.clone(), descriptor, context);
        self.store.append(
            self.session_id,
            EventPayload::InferenceProfileChanged {
                provider: profile.provider,
                model: profile.model,
                effort: profile.effort,
                reason: reason.to_owned(),
            },
        )?;
        Ok(())
    }

    /// Applies a profile restored from durable history without appending a
    /// duplicate event. Used by resume.
    pub fn restore_inference_profile(
        &mut self,
        provider: Arc<dyn ModelProvider>,
        profile: InferenceProfile,
        descriptor: &ModelDescriptor,
        context: ContextConfig,
    ) {
        self.apply_inference_profile(provider, profile, descriptor, context);
    }

    fn apply_inference_profile(
        &mut self,
        provider: Arc<dyn ModelProvider>,
        profile: InferenceProfile,
        descriptor: &ModelDescriptor,
        context: ContextConfig,
    ) {
        // The estimator, context window, cache accounting, and provider must
        // all move together; a partial switch would misprice the next request.
        let estimator = TokenEstimator::for_model(&profile.model);
        self.provider = provider;
        self.profile = profile.clone();
        self.input_modalities = descriptor.input_modalities.clone();
        if !self.input_modalities.contains(&InputModality::Text) {
            self.input_modalities.insert(0, InputModality::Text);
        }
        self.estimator = estimator;
        self.continuity.set_estimator(estimator);
        // A profile change invalidates architecture-level prefix accounting.
        self.last_request_signature = None;
        let window = descriptor
            .context_window_tokens
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        self.set_context_budget(context, window);
        if let Some(supervisor) = &self.supervisor {
            supervisor.set_provider(self.provider.clone());
            supervisor.update_settings(|settings| settings.profile = profile.clone());
        }
    }
    #[must_use]
    pub const fn estimator(&self) -> &TokenEstimator {
        &self.estimator
    }

    /// Provider-neutral input modalities of the effective model.
    #[must_use]
    pub fn input_modalities(&self) -> &[InputModality] {
        &self.input_modalities
    }

    /// Whether the effective model can receive image input.
    #[must_use]
    pub fn supports_image_input(&self) -> bool {
        self.input_modalities.contains(&InputModality::Image)
    }

    /// Fails locally, before any provider request, when input carries images
    /// the effective model cannot accept. The image is never silently dropped
    /// and the model is never switched behind the user's back.
    fn ensure_media_supported(&self, media: &[latch_protocol::MediaRef]) -> Result<()> {
        if media.is_empty() || self.supports_image_input() {
            return Ok(());
        }
        Err(anyhow!(
            "Current model does not accept image input. Choose a vision-capable model with /model."
        ))
    }
    /// Changes the effective mode and records it durably so `--resume`
    /// restores the mode the session actually transitioned to.
    pub fn set_mode(&mut self, mode: Mode) -> Result<()> {
        self.mode = mode;
        self.tools.set_mode(mode);
        self.store
            .append(self.session_id, EventPayload::ModeChanged { mode })?;
        Ok(())
    }
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }
    /// Changes the effective safety profile and records it durably so resume
    /// restores exactly the profile the session ended in.
    pub fn set_safety(&mut self, safety: Safety) -> Result<()> {
        self.tools.set_safety(safety);
        self.store
            .append(self.session_id, EventPayload::SafetyChanged { safety })?;
        Ok(())
    }
    #[must_use]
    pub fn safety(&self) -> Safety {
        self.tools.safety()
    }
    /// Changes the effective permission resolver and records it durably.
    pub fn set_permissions(&mut self, mode: PermissionMode) -> Result<()> {
        self.tools.set_permissions(mode);
        self.store
            .append(self.session_id, EventPayload::PermissionsChanged { mode })?;
        Ok(())
    }
    #[must_use]
    pub fn permissions(&self) -> PermissionMode {
        self.tools.permissions()
    }
    /// Restores resumed policy settings without appending new events.
    pub fn restore_policy(&mut self, safety: Safety, permissions: PermissionMode) {
        self.tools.set_safety(safety);
        self.tools.set_permissions(permissions);
    }
    #[must_use]
    pub fn state(&self) -> &latch_protocol::TaskState {
        self.state.state()
    }
    #[must_use]
    pub fn evidence(&self) -> &EvidenceLedger {
        &self.evidence
    }
    #[must_use]
    pub fn failure_lineages(&self) -> Vec<(String, u32)> {
        self.failures.active_lineages()
    }
    #[must_use]
    pub fn progress(&self) -> &ProgressSupervisor {
        &self.progress
    }
    pub fn restore_state(&mut self, state: latch_protocol::TaskState) {
        self.last_completion = Some(state.completion.clone());
        self.state = TaskStateManager::new(state);
    }
    pub fn restore_evidence(&mut self, evidence: Vec<latch_protocol::Evidence>) {
        self.evidence = EvidenceLedger::new(evidence);
    }
    pub fn context(&self, query: Option<&str>) -> Result<crate::continuity::MaterializedContext> {
        let prompt = PromptCompiler::compile(self.mode, &self.workspace)?;
        // `/context` has no extension context to include, but tool schemas are
        // part of every real request, so reserve for them here too.
        let reserved = self.estimator.estimate_tools(&self.tool_definitions());
        self.continuity.materialize(ContextRequest {
            session_id: self.session_id,
            state: self.state.state(),
            query,
            evidence: &self.evidence,
            failures: &self.failures,
            system: prompt.text,
            budget: self.materialize_budget(reserved),
            extension_context: "",
            reground: None,
        })
    }

    /// The runtime capabilities this session currently declares: kind, owner,
    /// lifetime, scope, and permission ceiling. Kernel-owned classes are always
    /// present; capability-gated surfaces (a future Computer backend, browser,
    /// or service exposure) appear only once their mechanism exists and is
    /// configured. Introspection is read-only and never grants anything.
    #[must_use]
    pub fn capabilities(&self) -> CapabilityRegistry {
        use crate::sandbox::{Capability, CapabilitySet};
        let session = CapabilityLifetime::Session(self.session_id);
        let owner = CapabilityOwner::Session(self.session_id);
        let workspace_scope = CapabilityScope::Workspace {
            root: self.workspace.clone(),
        };
        let mut workspace_permissions = CapabilitySet::new();
        workspace_permissions.insert(Capability::WorkspaceRead);
        let mut executor_permissions = workspace_permissions.clone();
        executor_permissions.insert(Capability::BuildArtifactWrite);
        if self.mode.can_mutate() {
            workspace_permissions.insert(Capability::WorkspaceSourceWrite);
            executor_permissions.insert(Capability::WorkspaceSourceWrite);
        }
        let mut registry = CapabilityRegistry::new();
        let mut declare = |id: &'static str,
                           kind: CapabilityKind,
                           owner: CapabilityOwner,
                           scope: CapabilityScope,
                           permissions: CapabilitySet| {
            registry
                .declare(CapabilityDescriptor {
                    id: CapabilityId::kernel(id),
                    kind,
                    owner,
                    lifetime: session.clone(),
                    scope,
                    permissions,
                })
                .expect("kernel capability declarations are unique and valid");
        };
        declare(
            "workspace.primary",
            CapabilityKind::Workspace,
            owner.clone(),
            workspace_scope.clone(),
            workspace_permissions,
        );
        declare(
            "executor.sandboxed",
            CapabilityKind::Executor,
            owner.clone(),
            workspace_scope,
            executor_permissions,
        );
        declare(
            "context.engine",
            CapabilityKind::Context,
            CapabilityOwner::Kernel,
            CapabilityScope::Session,
            CapabilitySet::new(),
        );
        declare(
            "tools.kernel",
            CapabilityKind::Tools,
            CapabilityOwner::Kernel,
            CapabilityScope::Session,
            CapabilitySet::new(),
        );
        declare(
            "artifacts.session",
            CapabilityKind::Artifacts,
            owner.clone(),
            CapabilityScope::Session,
            CapabilitySet::new(),
        );
        if self.supervisor.is_some() {
            declare(
                "agents.supervisor",
                CapabilityKind::Agents,
                CapabilityOwner::Kernel,
                CapabilityScope::Session,
                CapabilitySet::new(),
            );
        }
        registry
    }
    pub fn compact(&mut self) -> Result<()> {
        self.continuity.manual_compact(self.session_id)
    }
    pub async fn load_extension(
        &mut self,
        name: String,
        command: &str,
        args: &[String],
    ) -> Result<()> {
        // The extension host runs inside the mandatory sandbox. If the
        // sandbox is unavailable, loading fails instead of spawning an
        // unsandboxed process.
        let runner = self.tools.sandbox_runner_for_extension()?;
        let profile = self.tools.extension_sandbox_profile();
        self.extensions
            .add(
                name,
                command,
                args,
                &self.workspace.to_string_lossy(),
                Some((&runner, &profile)),
            )
            .await
    }
    pub async fn shutdown_extensions(&mut self) -> Result<()> {
        if let Some(supervisor) = &self.supervisor {
            supervisor.close_all().await?;
        }
        self.extensions.shutdown_all().await
    }

    #[must_use]
    pub fn agent_supervisor(&self) -> Option<AgentSupervisor> {
        self.supervisor.clone()
    }

    /// Installs the root-scoped group coordinator on any agent (root or child).
    pub(crate) fn set_group(&mut self, group: Option<GroupCoordinator>) {
        self.group = group;
    }

    /// The durable coordination overlay this agent participates in, when one
    /// has been provisioned by the root supervisor.
    #[must_use]
    pub fn group(&self) -> Option<GroupCoordinator> {
        self.group.clone()
    }

    pub(crate) fn agent_report(
        &self,
        identity: &AgentIdentity,
        status: AgentStatus,
        summary: String,
    ) -> AgentReport {
        let mut claims = Vec::<String>::new();
        let evidence = self
            .evidence
            .entries()
            .iter()
            .filter(|entry| {
                if claims
                    .iter()
                    .any(|claim| crate::state::same_text(claim, &entry.claim))
                {
                    false
                } else {
                    claims.push(entry.claim.clone());
                    true
                }
            })
            .filter_map(|entry| self.evidence.current(&entry.claim))
            .map(|entry| AgentEvidenceRef {
                claim: entry.claim.clone(),
                status: entry.status.clone(),
                detail: entry.detail.clone(),
            })
            .collect();
        let state = self.state.state();
        AgentReport {
            report_id: Uuid::new_v4(),
            agent_id: identity.agent_id,
            task_name: identity.task_name.clone(),
            status,
            completion: state.completion.clone(),
            summary: compact_agent_summary(&summary),
            findings: state.decisions.clone(),
            touched_files: state.touched_files.clone(),
            evidence,
            unresolved_questions: state.open_questions.clone(),
        }
    }
    /// Runs a kernel-owned builtin tool without forwarding appended events.
    /// Callers that render live state should prefer
    /// [`Self::builtin_tool_streamed`].
    pub async fn builtin_tool(&self, name: &str, cancel: CancellationToken) -> ToolResult {
        let silent: AgentEventSink = Arc::new(|_| {});
        self.builtin_tool_streamed(name, cancel, &silent).await
    }

    /// Runs a kernel-owned builtin tool and publishes any durable events it
    /// appends (`/undo` and friends) so live consumers stay in sync with
    /// replay.
    pub async fn builtin_tool_streamed(
        &self,
        name: &str,
        cancel: CancellationToken,
        sink: &AgentEventSink,
    ) -> ToolResult {
        let result = self
            .tools
            .execute(
                &latch_protocol::ToolCall {
                    id: format!("builtin-{}", Uuid::new_v4()),
                    name: name.into(),
                    arguments: serde_json::json!({}),
                },
                cancel,
            )
            .await;
        let _ = self.forward_appended_events(sink);
        result
    }
    /// Kernel-side validation with the same semantics as the model-facing
    /// `validate` tool: kernel-owned execution, provenance, evidence, and
    /// derived completion. Used by tests and tooling; the model always goes
    /// through the tool.
    pub async fn run_validation(
        &mut self,
        requirement: &str,
        command: &str,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let call = ToolCall {
            id: format!("kernel-validate-{}", Uuid::new_v4()),
            name: "validate".into(),
            arguments: json!({"requirement": requirement, "command": command}),
        };
        let sink: AgentEventSink = Arc::new(|_| {});
        self.execute_validate(&call, cancel, &sink).await
    }
    /// Records one user turn with normal provenance. Used for the initial
    /// prompt and for every live-steering message, so injected turns are
    /// indistinguishable from ordinary user turns in durable history. Images
    /// are durable media references; bytes stay in artifact storage.
    async fn record_user_message(
        &mut self,
        input: &UserInput,
        sink: &AgentEventSink,
    ) -> Result<()> {
        self.ensure_media_supported(&input.media)?;
        let user_event = self.emit(
            EventPayload::UserMessage {
                text: input.text.clone(),
                media: input.media.clone(),
            },
            sink,
        )?;
        if looks_like_constraint(&input.text) {
            self.store.add_memory(&MemoryRecord {
                id: Uuid::new_v4(),
                session_id: self.session_id,
                kind: MemoryKind::UserConstraint,
                content: input.text.clone(),
                originating_event: user_event.id,
                created_at: Utc::now(),
                validity: Validity::Active,
                confidence: None,
                dependencies: vec![],
                supersedes: None,
            })?;
        }
        self.extensions
            .observe(
                "user_message",
                json!({"text":input.text,"media":input.media.len(),"sessionId":self.session_id}),
            )
            .await?;
        if self.state.state().goal.is_empty() && !input.text.trim().is_empty() {
            self.state.update(crate::state::StateUpdate {
                goal: Some(input.text.clone()),
                ..Default::default()
            });
            self.emit(
                EventPayload::TaskStateUpdated {
                    state: self.state.state().clone(),
                },
                sink,
            )?;
        }
        Ok(())
    }

    /// Records newly drained steering messages in submission order. Each one
    /// stays a distinct durable `UserMessage`; the supervisors then observe the
    /// new user turns exactly like an ordinary prompt.
    async fn record_steers(&mut self, inputs: &[UserInput], sink: &AgentEventSink) -> Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        for input in inputs {
            self.record_user_message(input, sink).await?;
        }
        self.observe_progress_events()
    }

    async fn record_agent_messages(
        &mut self,
        messages: &[latch_protocol::AgentMessage],
        sink: &AgentEventSink,
    ) -> Result<()> {
        for message in messages {
            let event = self.emit(
                EventPayload::AgentMessageReceived {
                    message: message.clone(),
                },
                sink,
            )?;
            if looks_like_constraint(&message.text) {
                self.store.add_memory(&MemoryRecord {
                    id: Uuid::new_v4(),
                    session_id: self.session_id,
                    kind: MemoryKind::UserConstraint,
                    content: message.text.clone(),
                    originating_event: event.id,
                    created_at: Utc::now(),
                    validity: Validity::Active,
                    confidence: None,
                    dependencies: vec![],
                    supersedes: None,
                })?;
            }
        }
        self.observe_progress_events()
    }

    /// Runs one task turn to completion, consuming accepted live steering at
    /// safe model boundaries. The queue is opened for the whole run and closed
    /// atomically when the run makes its exit decision, so a submission always
    /// has one deterministic outcome: accepted-and-consumed or rejected.
    ///
    /// Text-only callers pass a plain string; structured callers pass a
    /// [`UserInput`] carrying durable image references.
    pub async fn run(
        &mut self,
        input: impl Into<UserInput>,
        cancel: CancellationToken,
        sink: AgentEventSink,
    ) -> Result<String> {
        let input = input.into();
        // Fail closed before any durable user turn is recorded, so an image a
        // text-only model cannot see never enters the conversation silently.
        self.ensure_media_supported(&input.media)?;
        self.steering.open();
        // Durable run boundary: every provider request, tool call, usage
        // record, and mutation between these events belongs to exactly one run.
        let run_id = Uuid::new_v4();
        self.emit(
            EventPayload::RunStarted {
                run_id,
                prompt: compact_agent_summary(&input.text),
            },
            &sink,
        )?;
        let result = self.run_loop(&input, cancel.clone(), sink.clone()).await;
        let outcome = match &result {
            Ok(_) => "completed",
            Err(_) if cancel.is_cancelled() => "cancelled",
            Err(_) => "error",
        };
        // RunCompleted commits even on failure: the boundary is provenance,
        // not a success claim. A storage failure here is returned instead of
        // masking the boundary.
        self.emit(
            EventPayload::RunCompleted {
                run_id,
                outcome: outcome.to_owned(),
            },
            &sink,
        )?;
        // Normal exits already atomically closed the queue at the final
        // answer. Aborted runs (cancel or error) close here, dropping any
        // accepted-but-unconsumed steer instead of leaking it into the next
        // run.
        self.steering.close();
        result
    }

    async fn run_loop(
        &mut self,
        input: &UserInput,
        cancel: CancellationToken,
        sink: AgentEventSink,
    ) -> Result<String> {
        // Everything appended from here on is fed to the supervisor and to the
        // live sink in order, exactly as a later replay would process it.
        let start = self.store.last_sequence(self.session_id)?;
        self.progress_watermark = start;
        self.forward_watermark.store(start, Ordering::Relaxed);
        let initial_agent_messages = self.child_mailbox.drain();
        let effective_user_text = if initial_agent_messages.is_empty() {
            self.record_user_message(input, &sink).await?;
            input.text.clone()
        } else {
            self.record_agent_messages(&initial_agent_messages, &sink)
                .await?;
            initial_agent_messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        if self.state.state().goal.is_empty() {
            self.state.update(crate::state::StateUpdate {
                goal: Some(effective_user_text.clone()),
                ..Default::default()
            });
            self.emit(
                EventPayload::TaskStateUpdated {
                    state: self.state.state().clone(),
                },
                &sink,
            )?;
        }
        // A new user turn is meaningful progress: re-observing anything is
        // legitimate again, and the stagnation bookkeeping restarts. The user
        // events are consumed through the same path replay uses so live and
        // resumed supervision stay identical.
        self.observe_progress_events()?;
        let mut final_text = String::new();
        let mut turns = 0u32;
        // Retrieval query for the next request. A newly injected steer is the
        // authority for what older material is relevant; ordinary continuation
        // turns leave this empty so they never trigger surprise recall.
        let mut steer_query: Option<String> = None;
        loop {
            turns += 1;
            self.terminal_complete = false;
            // The ultimate circuit breaker is opt-in and off by default: a
            // long-horizon task making real progress is never killed by turn
            // count. Stagnation and failure supervision are the primary loop
            // controls.
            if let Some(limit) = self.max_model_turns
                && turns > limit
            {
                return Err(anyhow!(
                    "agent exceeded the configured model-turn circuit breaker ({limit})"
                ));
            }
            // Safe model boundary: every prior tool transaction has a terminal
            // result. Drain live steering here, before the next request exists,
            // recording each queued message as a normal durable user turn in
            // the order it was submitted.
            let queued = self.steering.drain();
            if !queued.is_empty() {
                self.record_steers(&queued, &sink).await?;
                steer_query = Some(steer_query_text(&queued));
            }
            let agent_messages = self.child_mailbox.drain();
            if !agent_messages.is_empty() {
                self.record_agent_messages(&agent_messages, &sink).await?;
                steer_query = Some(
                    agent_messages
                        .iter()
                        .map(|message| message.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            self.deliver_agent_notifications(&sink)?;
            // Group messages are delivered at the same safe boundary as child
            // reports: never between an assistant tool call and its results.
            self.deliver_group_messages(&sink)?;
            let query = steer_query
                .take()
                .or_else(|| (turns == 1).then(|| effective_user_text.clone()));

            // Budget the complete request: tool schemas and extension context
            // are part of every call, so they are reserved before the
            // continuity engine allocates its own sections.
            let extension_context = self.extensions.context(&cancel).await?;
            let extension_json = serde_json::to_string_pretty(&extension_context)?;
            let tools = self.tool_definitions();
            let tools_tokens = self.estimator.estimate_tools(&tools);
            let extension_tokens = self.estimator.estimate(&extension_json);
            let budget = self.materialize_budget(tools_tokens.saturating_add(extension_tokens));
            let ctx = self.continuity.materialize(ContextRequest {
                session_id: self.session_id,
                state: self.state.state(),
                query: query.as_deref(),
                evidence: &self.evidence,
                failures: &self.failures,
                system: PromptCompiler::compile(self.mode, &self.workspace)?.text,
                budget,
                extension_context: &extension_json,
                reground: self.progress.reground_instruction().as_deref(),
            })?;
            let mut stats = ctx.stats.clone();
            stats.tools_tokens = tools_tokens;
            stats.recompute();
            // Kernel-owned context is durable history now: every request within
            // a cache epoch is an append-only extension of the previous one.
            let messages = context_messages(&ctx);
            let request = ModelRequest {
                system: ctx.system.clone(),
                messages,
                tools,
            };
            let request: ModelRequest = serde_json::from_value(
                self.extensions
                    .transform("model_request", serde_json::to_value(request)?, &cancel)
                    .await?,
            )
            .context("extension returned invalid model_request transform")?;
            // Replayed history may contain images from an earlier model. Fail
            // locally before the provider request instead of dropping them or
            // letting the endpoint reject the request ambiguously.
            if request.messages.iter().any(ModelMessage::has_media) && !self.supports_image_input()
            {
                return Err(anyhow!(
                    "Current model does not accept image input. Choose a vision-capable model with /model."
                ));
            }
            // Diagnostics for the exact provider-facing shape: the real request
            // size (including tool calls, arguments, and replayed reasoning)
            // and the estimated architecture cacheability — the byte prefix
            // shared with the previous request under Latch's own serialization
            // and estimator, not the provider's tokenizer. Provider-reported
            // cache usage stays authoritative.
            let signature = request_signature(&request);
            stats.request_tokens = self
                .estimator
                .estimate(&request.system)
                .saturating_add(self.estimator.estimate_messages(&request.messages))
                .saturating_add(self.estimator.estimate_tools(&request.tools));
            stats.common_prefix_tokens = match &self.last_request_signature {
                Some(previous) => {
                    let shared = common_prefix_bytes(previous, &signature);
                    self.estimator.estimate(&signature[..shared])
                }
                None => 0,
            };
            self.last_request_signature = Some(signature);
            self.emit(
                EventPayload::ContextMaterialized {
                    stats: stats.clone(),
                },
                &sink,
            )?;
            self.emit(
                EventPayload::ModelRequestStarted {
                    provider: self.provider.name().into(),
                    model: self.provider.model().into(),
                },
                &sink,
            )?;
            let transient = sink.clone();
            let provider_sink: StreamSink = Arc::new(move |e| transient(AgentOutput::Transient(e)));
            let response = match self
                .call_with_retry(request, cancel.clone(), provider_sink)
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.emit(
                        EventPayload::ModelRequestFinished {
                            stop_reason: "error".into(),
                        },
                        &sink,
                    )?;
                    sink(AgentOutput::Transient(StreamEvent::Error(
                        error.to_string(),
                    )));
                    return Err(error);
                }
            };
            final_text.push_str(&response.text);
            self.emit(
                EventPayload::AssistantMessageCompleted {
                    text: response.text.clone(),
                    tool_calls: response.tool_calls.clone(),
                    reasoning_content: response.reasoning_content.clone(),
                    reasoning: response.reasoning.clone(),
                },
                &sink,
            )?;
            self.emit(
                EventPayload::ModelRequestFinished {
                    stop_reason: response.stop_reason.clone(),
                },
                &sink,
            )?;
            if let Some(usage) = response.usage {
                self.emit(EventPayload::ModelUsage { usage }, &sink)?;
            }
            if response.tool_calls.is_empty() {
                // Atomic run-closing handshake. A steering message submitted
                // while the model was streaming its final answer must either be
                // consumed by this run or rejected; it can never be left in the
                // queue for a later run.
                let late = self.steering.close_and_drain();
                let agent_messages = self.child_mailbox.drain();
                if !agent_messages.is_empty() {
                    self.record_agent_messages(&agent_messages, &sink).await?;
                    steer_query = Some(
                        agent_messages
                            .iter()
                            .map(|message| message.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                let notifications = self.deliver_agent_notifications(&sink)?;
                let group_messages = self.deliver_group_messages(&sink)?;
                if late.is_empty()
                    && agent_messages.is_empty()
                    && notifications == 0
                    && group_messages == 0
                {
                    break;
                }
                if !late.is_empty() {
                    self.record_steers(&late, &sink).await?;
                    steer_query = Some(steer_query_text(&late));
                }
                continue;
            }
            for call in &response.tool_calls {
                self.emit(EventPayload::ToolRequested { call: call.clone() }, &sink)?;
            }
            let calls = response.tool_calls.clone();
            let tool_results = self.execute_batch(calls, cancel.clone(), &sink).await?;
            // Publish tool-appended durable events before the display results,
            // keeping live consumers in exact durable order.
            self.forward_appended_events(&sink)?;
            for result in &tool_results {
                sink(AgentOutput::ToolResult(result.clone()));
            }
            self.supervise_failures(&response.tool_calls, &tool_results, &sink)?;
            self.supervise_progress(&sink)?;
            // Terminal-complete fast path: when this turn executed `complete`
            // and the kernel-derived completion is terminal, the assistant text
            // emitted in the same turn is the final answer. Do not spend another
            // provider request on a summary that already exists. The safe
            // boundary handshake still applies: accepted steering, child
            // messages, and undelivered child reports keep the run open.
            if self.terminal_complete {
                self.terminal_complete = false;
                let late = self.steering.close_and_drain();
                let agent_messages = self.child_mailbox.drain();
                if !agent_messages.is_empty() {
                    self.record_agent_messages(&agent_messages, &sink).await?;
                    steer_query = Some(
                        agent_messages
                            .iter()
                            .map(|message| message.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                let notifications = self.deliver_agent_notifications(&sink)?;
                let group_messages = self.deliver_group_messages(&sink)?;
                if late.is_empty()
                    && agent_messages.is_empty()
                    && notifications == 0
                    && group_messages == 0
                {
                    break;
                }
                if !late.is_empty() {
                    self.record_steers(&late, &sink).await?;
                    steer_query = Some(steer_query_text(&late));
                }
                continue;
            }
        }
        Ok(final_text)
    }
    async fn call_with_retry(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<latch_protocol::ModelResponse> {
        let mut last = None;
        for attempt in 0..=self.max_model_retries {
            match self
                .provider
                .stream(request.clone(), cancel.clone(), sink.clone())
                .await
            {
                Ok(r) => return Ok(r),
                Err(e) if attempt < self.max_model_retries && !cancel.is_cancelled() => {
                    self.store.append(
                        self.session_id,
                        EventPayload::FailureAttempt {
                            signature: format!("provider:{}", self.provider.name()),
                            count: attempt + 1,
                        },
                    )?;
                    last = Some(e);
                    tokio::select! {
                        () = tokio::time::sleep(
                            std::time::Duration::from_millis(100 * 2u64.pow(attempt)),
                        ) => {}
                        () = cancel.cancelled() => {
                            return Err(anyhow!("model request cancelled"));
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("model request failed")))
    }

    fn emit(&mut self, payload: EventPayload, sink: &AgentEventSink) -> Result<Event> {
        // Tool execution appends durable events (mutations, drift detection,
        // lifecycle) directly to the store. Deliver those to the live sink
        // first so consumers observe the exact durable order that replay sees.
        self.forward_appended_events(sink)?;
        let event = self.store.append(self.session_id, payload)?;
        sink(AgentOutput::Durable(Box::new(event.clone())));
        self.forward_watermark
            .store(event.sequence, Ordering::Relaxed);
        Ok(event)
    }
    /// Forwards durable events appended since the watermark to the live sink.
    /// This keeps live presentation and sidebar state in sync with events that
    /// never pass through [`Self::emit`], such as `FileChanged` or
    /// `ExternalFileChangeDetected`.
    fn forward_appended_events(&self, sink: &AgentEventSink) -> Result<()> {
        let watermark = self.forward_watermark.load(Ordering::Relaxed);
        let last = self.store.last_sequence(self.session_id)?;
        if last <= watermark {
            if last < watermark {
                self.forward_watermark.store(last, Ordering::Relaxed);
            }
            return Ok(());
        }
        let events = self.store.events_after(self.session_id, watermark)?;
        for event in &events {
            sink(AgentOutput::Durable(Box::new(event.clone())));
        }
        self.forward_watermark.store(last, Ordering::Relaxed);
        Ok(())
    }
}

fn steer_query_text(inputs: &[UserInput]) -> String {
    inputs
        .iter()
        .map(|input| input.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn looks_like_constraint(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "must ",
        "must not",
        "do not",
        "don't ",
        "never ",
        "required",
        "constraint",
        "preserve ",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn compact_agent_summary(text: &str) -> String {
    const LIMIT: usize = 2_000;
    let text = text.trim();
    if text.chars().count() <= LIMIT {
        return text.to_owned();
    }
    let mut summary = text
        .chars()
        .take(LIMIT.saturating_sub(1))
        .collect::<String>();
    summary.push('…');
    summary
}
#[cfg(test)]
mod tests;
