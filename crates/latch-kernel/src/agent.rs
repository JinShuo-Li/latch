use crate::config::{ContextConfig, DEFAULT_CONTEXT_WINDOW_TOKENS};
use crate::continuity::{ContinuityEngine, MaterializeBudget};
use crate::extension::{ExtensionGuardDecision, ExtensionRegistry};
use crate::permissions::PermissionBroker;
use crate::progress::{DEFAULT_STAGNATION_BUDGET, ProgressSupervisor, StagnationDecision};
use crate::prompt::PromptCompiler;
use crate::provider::{ModelProvider, StreamSink};
use crate::safety::{Classification, Decision as SafetyDecision};
use crate::state::{
    EvidenceLedger, FailureManager, StateUpdate, TaskStateManager, failure_subject,
};
use crate::store::EventStore;
use crate::tokens::TokenEstimator;
use crate::tools::{CapabilityGrant, ToolExecutor};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use latch_protocol::{
    CompletionState, Event, EventPayload, EvidenceStatus, MemoryKind, MemoryRecord, Mode,
    ModelMessage, ModelRequest, PermissionMode, Safety, StreamEvent, ToolCall, ToolDefinition,
    ToolResult, Validity,
};
use request::{common_prefix_bytes, context_messages, request_signature};
use serde_json::json;
use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod dispatch;
mod kernel_tools;
mod permissions;
mod request;
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
    continuity: ContinuityEngine,
    state: TaskStateManager,
    evidence: EvidenceLedger,
    extensions: ExtensionRegistry,
    failures: FailureManager,
    progress: ProgressSupervisor,
    /// Sequence cursor of the last event already consumed by the progress
    /// supervisor; per-turn supervision only reads strictly newer events.
    progress_watermark: u64,
    /// Sequence cursor of the last event already forwarded to the live sink.
    forward_watermark: Cell<u64>,
    /// Call ids whose terminal result was produced by the kernel rather than
    /// by model failure (progress suppression or steering supersession).
    /// Failure supervision must not count them against the model.
    kernel_resolved_calls: HashSet<String>,
    max_model_retries: u32,
    max_model_turns: Option<u32>,
    context_window_tokens: usize,
    estimator: TokenEstimator,
    permissions: PermissionBroker,
    /// Messages queued while the loop is running; drained only at safe model
    /// boundaries.
    steering: SteeringQueue,
    /// Canonical serialization of the previous provider-facing request, used to
    /// measure the exact reusable common prefix.
    last_request_signature: Option<String>,
    interactive_permissions: bool,
    last_completion: Option<CompletionState>,
}
pub struct AgentRuntime {
    pub session_id: Uuid,
    pub workspace: PathBuf,
    pub mode: Mode,
    pub store: EventStore,
    pub provider: Arc<dyn ModelProvider>,
    pub tools: ToolExecutor,
    pub continuity: ContinuityEngine,
    pub retry_budget: u32,
}
impl Agent {
    #[must_use]
    pub fn new(runtime: AgentRuntime) -> Self {
        let estimator = TokenEstimator::for_model(runtime.provider.model());
        let mut continuity = runtime.continuity;
        // The request estimator and the continuity budget must agree on the
        // provider model.
        continuity.set_estimator(estimator);
        let progress =
            ProgressSupervisor::new(DEFAULT_STAGNATION_BUDGET, runtime.workspace.clone());
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
            forward_watermark: Cell::new(0),
            kernel_resolved_calls: HashSet::new(),
            max_model_retries: 2,
            max_model_turns: None,
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            estimator,
            permissions: PermissionBroker::new(),
            steering: SteeringQueue::new(),
            last_request_signature: None,
            interactive_permissions: false,
            last_completion: None,
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
    /// Resolves a pending approval. Returns false when the request is unknown
    /// or already resolved, so a stale or forged id can never approve twice.
    pub async fn resolve_permission(&self, request_id: Uuid, approved: bool) -> bool {
        self.permissions.resolve(request_id, approved).await
    }
    /// Configures how many consecutive redundant inspection turns are
    /// tolerated before the kernel re-grounds the model.
    pub fn set_stagnation_budget(&mut self, budget: u32) {
        self.progress.set_budget(budget);
    }
    /// Sets the ultimate model-turn circuit breaker. `None` keeps long,
    /// productive tasks unlimited; the stagnation and failure supervisors
    /// remain the primary loop controls.
    pub fn set_max_model_turns(&mut self, max_model_turns: Option<u32>) {
        self.max_model_turns = max_model_turns;
    }
    /// Installs the token-native context configuration and the resolved model
    /// context window.
    pub fn set_context_budget(&mut self, context: ContextConfig, window_tokens: usize) {
        self.continuity.set_config(context);
        self.context_window_tokens = window_tokens.max(1);
    }
    #[must_use]
    pub const fn estimator(&self) -> &TokenEstimator {
        &self.estimator
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
        self.continuity.materialize(
            self.session_id,
            self.state.state(),
            query,
            &self.evidence,
            &self.failures,
            prompt.text,
            &self.materialize_budget(reserved),
        )
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
        self.extensions.shutdown_all().await
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
        Ok(self.execute_validate(&call, cancel, &sink).await)
    }
    /// Records one user turn with normal provenance. Used for the initial
    /// prompt and for every live-steering message, so injected turns are
    /// indistinguishable from ordinary user turns in durable history.
    async fn record_user_message(&mut self, text: &str, sink: &AgentEventSink) -> Result<()> {
        let user_event = self.emit(EventPayload::UserMessage { text: text.into() }, sink)?;
        if looks_like_constraint(text) {
            self.store.add_memory(&MemoryRecord {
                id: Uuid::new_v4(),
                session_id: self.session_id,
                kind: MemoryKind::UserConstraint,
                content: text.into(),
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
                json!({"text":text,"sessionId":self.session_id}),
            )
            .await?;
        if self.state.state().goal.is_empty() {
            self.state.update(crate::state::StateUpdate {
                goal: Some(text.into()),
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
    async fn record_steers(&mut self, texts: &[String], sink: &AgentEventSink) -> Result<()> {
        if texts.is_empty() {
            return Ok(());
        }
        for text in texts {
            self.record_user_message(text, sink).await?;
        }
        self.observe_progress_events()
    }

    /// Runs one task turn to completion, consuming accepted live steering at
    /// safe model boundaries. The queue is opened for the whole run and closed
    /// atomically when the run makes its exit decision, so a submission always
    /// has one deterministic outcome: accepted-and-consumed or rejected.
    pub async fn run(
        &mut self,
        user_text: &str,
        cancel: CancellationToken,
        sink: AgentEventSink,
    ) -> Result<String> {
        self.steering.open();
        let result = self.run_loop(user_text, cancel, sink).await;
        // Normal exits already atomically closed the queue at the final
        // answer. Aborted runs (cancel or error) close here, dropping any
        // accepted-but-unconsumed steer instead of leaking it into the next
        // run.
        self.steering.close();
        result
    }

    async fn run_loop(
        &mut self,
        user_text: &str,
        cancel: CancellationToken,
        sink: AgentEventSink,
    ) -> Result<String> {
        // Everything appended from here on is fed to the supervisor and to the
        // live sink in order, exactly as a later replay would process it.
        let start = self.store.last_sequence(self.session_id)?;
        self.progress_watermark = start;
        self.forward_watermark.set(start);
        self.record_user_message(user_text, &sink).await?;
        if self.state.state().goal.is_empty() {
            self.state.update(crate::state::StateUpdate {
                goal: Some(user_text.into()),
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
                steer_query = Some(queued.join("\n"));
            }
            let query = steer_query
                .take()
                .or_else(|| (turns == 1).then(|| user_text.to_owned()));

            // Budget the complete request: tool schemas and extension context
            // are part of every call, so they are reserved before the
            // continuity engine allocates its own sections.
            let extension_context = self.extensions.context().await?;
            let extension_json = serde_json::to_string_pretty(&extension_context)?;
            let tools = self.tool_definitions();
            let tools_tokens = self.estimator.estimate_tools(&tools);
            let extension_tokens = self.estimator.estimate(&extension_json);
            let budget = self.materialize_budget(tools_tokens.saturating_add(extension_tokens));
            let ctx = self.continuity.materialize(
                self.session_id,
                self.state.state(),
                query.as_deref(),
                &self.evidence,
                &self.failures,
                PromptCompiler::compile(self.mode, &self.workspace)?.text,
                &budget,
            )?;
            let mut stats = ctx.stats.clone();
            stats.tools_tokens = tools_tokens;
            stats.extension_tokens = extension_tokens;
            stats.recompute();
            let mut messages = context_messages(&ctx);
            // Kernel-owned re-ground: while stagnation supervision is active,
            // the model receives the explicit list of unchanged observations.
            if let Some(instruction) = self.progress.reground_instruction() {
                messages.push(ModelMessage::text("user", instruction));
            }
            // The compiled system prompt is the stable, cacheable prefix. The
            // frequently changing canonical state, recalled originals, and
            // extension context travel as a final kernel context turn, so
            // ordinary task-state/evidence updates cannot invalidate the
            // reusable system + tools + conversation prefix.
            let kernel_context = format!(
                "Kernel context (authoritative current state; not a new request):\n\n{}\n\nRECALLED ORIGINAL MATERIAL\n{}\n\nEXTENSION CONTEXT SOURCES\n{}",
                ctx.canonical, ctx.recalled, extension_json
            );
            messages.push(ModelMessage {
                role: "user".into(),
                content: kernel_context,
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
            });
            let request = ModelRequest {
                system: ctx.system.clone(),
                messages,
                tools,
            };
            let request: ModelRequest = serde_json::from_value(
                self.extensions
                    .transform("model_request", serde_json::to_value(request)?)
                    .await?,
            )
            .context("extension returned invalid model_request transform")?;
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
                if late.is_empty() {
                    break;
                }
                self.record_steers(&late, &sink).await?;
                steer_query = Some(late.join("\n"));
                continue;
            }
            for call in &response.tool_calls {
                self.emit(EventPayload::ToolRequested { call: call.clone() }, &sink)?;
            }
            let calls = response.tool_calls.clone();
            let tool_results = self.execute_batch(calls, cancel.clone(), &sink).await;
            // Publish tool-appended durable events before the display results,
            // keeping live consumers in exact durable order.
            self.forward_appended_events(&sink)?;
            for result in &tool_results {
                sink(AgentOutput::ToolResult(result.clone()));
            }
            self.supervise_failures(&response.tool_calls, &tool_results, &sink)?;
            self.supervise_progress(&sink)?;
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
                    tokio::time::sleep(std::time::Duration::from_millis(100 * 2u64.pow(attempt)))
                        .await;
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
        self.forward_watermark.set(event.sequence);
        Ok(event)
    }
    /// Forwards durable events appended since the watermark to the live sink.
    /// This keeps live presentation and sidebar state in sync with events that
    /// never pass through [`Self::emit`], such as `FileChanged` or
    /// `ExternalFileChangeDetected`.
    fn forward_appended_events(&self, sink: &AgentEventSink) -> Result<()> {
        let watermark = self.forward_watermark.get();
        let last = self.store.last_sequence(self.session_id)?;
        if last <= watermark {
            if last < watermark {
                self.forward_watermark.set(last);
            }
            return Ok(());
        }
        let events = self.store.events_after(self.session_id, watermark)?;
        for event in &events {
            sink(AgentOutput::Durable(Box::new(event.clone())));
        }
        self.forward_watermark.set(last);
        Ok(())
    }
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
#[cfg(test)]
mod tests;
