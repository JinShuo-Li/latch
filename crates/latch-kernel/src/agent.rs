use crate::continuity::ContinuityEngine;
use crate::extension::{ExtensionGuardDecision, ExtensionRegistry};
use crate::progress::{DEFAULT_STAGNATION_BUDGET, ProgressSupervisor, StagnationDecision};
use crate::prompt::PromptCompiler;
use crate::provider::{ModelProvider, StreamSink};
use crate::state::{
    EvidenceLedger, FailureManager, StateUpdate, TaskStateManager, failure_subject,
};
use crate::store::EventStore;
use crate::tools::ToolExecutor;
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use latch_protocol::{
    CompletionState, Event, EventPayload, EvidenceStatus, MemoryKind, MemoryRecord, Mode,
    ModelMessage, ModelRequest, StreamEvent, ToolCall, ToolDefinition, ToolResult, Validity,
};
use serde_json::json;
use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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
    progress_watermark: usize,
    forward_watermark: Cell<usize>,
    suppressed_calls: HashSet<String>,
    max_model_retries: u32,
    scope_warned: bool,
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
        let progress =
            ProgressSupervisor::new(DEFAULT_STAGNATION_BUDGET, runtime.workspace.clone());
        Self {
            session_id: runtime.session_id,
            workspace: runtime.workspace,
            mode: runtime.mode,
            store: runtime.store,
            provider: runtime.provider,
            tools: runtime.tools,
            continuity: runtime.continuity,
            state: TaskStateManager::default(),
            evidence: EvidenceLedger::default(),
            extensions: ExtensionRegistry::new(),
            failures: FailureManager::new(runtime.retry_budget),
            progress,
            progress_watermark: 0,
            forward_watermark: Cell::new(0),
            suppressed_calls: HashSet::new(),
            max_model_retries: 2,
            scope_warned: false,
            last_completion: None,
        }
    }
    /// Configures how many consecutive redundant inspection turns are
    /// tolerated before the kernel re-grounds the model.
    pub fn set_stagnation_budget(&mut self, budget: u32) {
        self.progress.set_budget(budget);
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
    /// Rebuilds failure supervision from the durable event log so a stalled
    /// validation loop survives `--resume`.
    pub fn restore_failures(&mut self) -> Result<()> {
        let events = self.store.events(self.session_id)?;
        let mut calls: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        let mut attempts: Vec<(String, bool, String)> = Vec::new();
        for event in &events {
            match &event.payload {
                EventPayload::ToolRequested { call } => {
                    calls.insert(
                        call.id.clone(),
                        (
                            call.name.clone(),
                            failure_subject(&call.name, &call.arguments),
                        ),
                    );
                }
                EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                    if let Some((tool, subject)) = calls.get(&result.call_id)
                        && matches!(tool.as_str(), "shell" | "validate")
                    {
                        let failed = matches!(&event.payload, EventPayload::ToolFailed { .. });
                        attempts.push((subject.clone(), failed, result.output.clone()));
                    }
                }
                _ => {}
            }
        }
        self.failures.replay(
            attempts
                .iter()
                .map(|(subject, failed, output)| (subject.as_str(), *failed, output.as_str())),
        );
        Ok(())
    }
    /// Rebuilds progress/stagnation supervision from the durable event log so
    /// `--resume` does not immediately forget an active inspection loop. The
    /// watermark advances past everything already consumed.
    pub fn restore_progress(&mut self) -> Result<()> {
        let events = self.store.events(self.session_id)?;
        self.progress.reset();
        self.progress.replay(&events);
        self.progress_watermark = events.len();
        Ok(())
    }
    pub fn context(&self, query: Option<&str>) -> Result<crate::continuity::MaterializedContext> {
        let prompt = PromptCompiler::compile(self.mode, self.state.state(), &self.workspace)?;
        self.continuity.materialize(
            self.session_id,
            self.state.state(),
            query,
            &self.evidence,
            &self.failures,
            prompt.text,
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
        self.extensions
            .add(name, command, args, &self.workspace.to_string_lossy())
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
    pub async fn run(
        &mut self,
        user_text: &str,
        cancel: CancellationToken,
        sink: AgentEventSink,
    ) -> Result<String> {
        // Everything appended from here on is fed to the supervisor and to the
        // live sink in order, exactly as a later replay would process it.
        let start = self.store.events(self.session_id)?.len();
        self.progress_watermark = start;
        self.forward_watermark.set(start);
        let user_event = self.emit(
            EventPayload::UserMessage {
                text: user_text.into(),
            },
            &sink,
        )?;
        // A constraint stated by the user carries UserConstraint provenance
        // because its originating event is an actual user message.
        if looks_like_constraint(user_text) {
            self.store.add_memory(&MemoryRecord {
                id: Uuid::new_v4(),
                session_id: self.session_id,
                kind: MemoryKind::UserConstraint,
                content: user_text.into(),
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
                json!({"text":user_text,"sessionId":self.session_id}),
            )
            .await?;
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
        loop {
            turns += 1;
            if turns > 32 {
                return Err(anyhow!("agent exceeded 32 tool turns"));
            }
            let query = if turns == 1 { Some(user_text) } else { None };
            let ctx = self.context(query)?;
            self.emit(
                EventPayload::ContextMaterialized {
                    stats: ctx.stats.clone(),
                },
                &sink,
            )?;
            let mut messages = context_messages(&ctx);
            // Kernel-owned re-ground: while stagnation supervision is active,
            // the model receives the explicit list of unchanged observations.
            if let Some(instruction) = self.progress.reground_instruction() {
                messages.push(ModelMessage::text("user", instruction));
            }
            let extension_context = self.extensions.context().await?;
            let request = ModelRequest {
                system: format!(
                    "{}\n\n{}\n\nRECALLED ORIGINAL MATERIAL\n{}\n\nEXTENSION CONTEXT SOURCES\n{}",
                    ctx.system,
                    ctx.canonical,
                    ctx.recalled,
                    serde_json::to_string_pretty(&extension_context)?
                ),
                messages,
                tools: self.tool_definitions(),
            };
            let request: ModelRequest = serde_json::from_value(
                self.extensions
                    .transform("model_request", serde_json::to_value(request)?)
                    .await?,
            )
            .context("extension returned invalid model_request transform")?;
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
                break;
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
            let scope = self.tools.scope_stats().await;
            if !self.scope_warned
                && (scope.files > 8
                    || scope.additions + scope.deletions > 500
                    || scope.dependency_files > 1)
            {
                self.emit(EventPayload::ScopeExpansionRequested { mutations: scope.mutations, reason: format!("Scope reached {} files, +{} -{} lines, and {} dependency manifests. Explain why this expansion is required by the user task before continuing.", scope.files, scope.additions, scope.deletions, scope.dependency_files) }, &sink)?;
                self.scope_warned = true;
            }
            self.supervise_failures(&response.tool_calls, &tool_results, &sink)?;
            self.supervise_progress(&sink)?;
        }
        Ok(final_text)
    }
    /// Feeds every durable event appended since the watermark to the progress
    /// supervisor and advances the watermark. Live supervision and replay
    /// consume the same event stream in the same order.
    fn observe_progress_events(&mut self) -> Result<()> {
        let events = self.store.events(self.session_id)?;
        if self.progress_watermark > events.len() {
            self.progress_watermark = 0;
        }
        for event in &events[self.progress_watermark..] {
            self.progress.observe_event(event);
        }
        self.progress_watermark = events.len();
        Ok(())
    }
    /// Deterministic inspection-loop supervision. Every durable event produced
    /// since the last turn is fed to the supervisor, the turn is settled, and a
    /// crossed stagnation budget injects a kernel-owned re-ground instruction
    /// on the next request.
    fn supervise_progress(&mut self, sink: &AgentEventSink) -> Result<()> {
        self.observe_progress_events()?;
        match self.progress.finish_turn() {
            Some(StagnationDecision::Reground {
                unchanged,
                redundant_turns,
            }) => {
                self.emit(
                    EventPayload::ProgressStagnation {
                        unchanged,
                        redundant_turns,
                    },
                    sink,
                )?;
            }
            None => {}
        }
        Ok(())
    }
    /// Failure supervision keyed by validation lineage: failed attempts
    /// escalate their own subject toward re-ground, and only a passing
    /// validation (or a materially changed failure signature, handled inside
    /// the manager) resolves the streak. Successful inspection tools never
    /// touch it.
    fn supervise_failures(
        &mut self,
        calls: &[ToolCall],
        results: &[ToolResult],
        sink: &AgentEventSink,
    ) -> Result<()> {
        for result in results {
            let Some(call) = calls.iter().find(|call| call.id == result.call_id) else {
                continue;
            };
            // Validations supervise themselves inside execute_validate so the
            // model path and the kernel path cannot double count. Kernel-
            // suppressed redundant observations are not tool failures.
            if call.name == "validate" || self.suppressed_calls.contains(&result.call_id) {
                continue;
            }
            let subject = failure_subject(&call.name, &call.arguments);
            if result.is_error {
                let decision = self.failures.record(&subject, &result.output);
                self.emit(
                    EventPayload::FailureAttempt {
                        signature: decision.signature,
                        count: decision.count,
                    },
                    sink,
                )?;
                if decision.reground {
                    self.emit(EventPayload::RegroundRequested { signature: subject }, sink)?;
                }
            } else if call.name == "shell" {
                self.failures.resolve(&subject);
            }
        }
        Ok(())
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
    async fn execute_batch(
        &mut self,
        calls: Vec<ToolCall>,
        cancel: CancellationToken,
        sink: &AgentEventSink,
    ) -> Vec<ToolResult> {
        let mut permitted = Vec::new();
        let mut results = Vec::new();
        for call in calls {
            match self
                .extensions
                .guard(
                    "tool.execute",
                    json!({"name":call.name,"arguments":call.arguments}),
                )
                .await
            {
                Ok(ExtensionGuardDecision::Allow) => permitted.push(call),
                Ok(ExtensionGuardDecision::Deny(reason) | ExtensionGuardDecision::Ask(reason)) => {
                    let denied = tool_error(&call, reason.clone());
                    let _ = self.emit(
                        EventPayload::PermissionDecision {
                            tool: call.name.clone(),
                            decision: "extension_guard".into(),
                            reason,
                        },
                        sink,
                    );
                    let _ = self.emit(
                        EventPayload::ToolFailed {
                            result: denied.clone(),
                        },
                        sink,
                    );
                    results.push(denied);
                }
                Err(error) => {
                    let failed = tool_error(&call, format!("extension guard failed: {error}"));
                    let _ = self.emit(
                        EventPayload::ToolFailed {
                            result: failed.clone(),
                        },
                        sink,
                    );
                    results.push(failed);
                }
            }
        }
        // Deterministic suppression: after the model ignored an explicit
        // re-ground, repeated observations of unchanged reality are rejected
        // with a synthetic terminal result instead of spending a tool cycle.
        // The lifecycle invariant still holds: every ToolRequested(call_id)
        // gets exactly one terminal result.
        self.suppressed_calls.clear();
        if self.progress.regrounded() {
            let mut allowed = Vec::with_capacity(permitted.len());
            for call in permitted {
                match self.progress.suppression_reason(&call) {
                    Some(label) => {
                        let suppressed = tool_error(
                            &call,
                            format!(
                                "Kernel suppressed redundant observation `{label}`: its result is unchanged since the last observation in this progress epoch. Use the existing result, act on it, or state the concrete blocker."
                            ),
                        );
                        self.suppressed_calls.insert(call.id.clone());
                        let _ = self.emit(
                            EventPayload::ToolFailed {
                                result: suppressed.clone(),
                            },
                            sink,
                        );
                        results.push(suppressed);
                    }
                    None => allowed.push(call),
                }
            }
            permitted = allowed;
        }
        let mut executed = if permitted.iter().all(|c| {
            matches!(
                c.name.as_str(),
                "read_file" | "search" | "git_status" | "git_diff"
            )
        }) {
            let tasks = permitted
                .into_iter()
                .map(|call| {
                    let tools = self.tools.clone();
                    let c = cancel.clone();
                    tokio::spawn(async move { tools.execute(&call, c).await })
                })
                .collect::<Vec<_>>();
            let mut batch = Vec::new();
            for task in tasks {
                match task.await {
                    Ok(r) => batch.push(r),
                    Err(e) => batch.push(ToolResult {
                        call_id: "join".into(),
                        name: "scheduler".into(),
                        output: e.to_string(),
                        is_error: true,
                        artifact_id: None,
                    }),
                }
            }
            batch
        } else {
            let mut batch = Vec::new();
            for call in permitted {
                if matches!(
                    call.name.as_str(),
                    "task_update" | "record_evidence" | "complete"
                ) {
                    batch.push(self.execute_kernel_tool(&call, sink));
                } else if call.name == "validate" {
                    batch.push(self.execute_validate(&call, cancel.clone(), sink).await);
                } else if let Some(owner) = self.extensions.owner_for_tool(&call.name) {
                    batch.push(self.execute_extension_tool(&owner, &call, sink).await);
                } else {
                    batch.push(self.tools.execute(&call, cancel.clone()).await);
                }
            }
            batch
        };
        results.append(&mut executed);
        results
    }
    async fn execute_extension_tool(
        &mut self,
        owner: &str,
        call: &ToolCall,
        sink: &AgentEventSink,
    ) -> ToolResult {
        if let Err(error) = self.emit(
            EventPayload::ToolStarted {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            },
            sink,
        ) {
            return tool_error(call, error.to_string());
        }
        let result = match self
            .extensions
            .execute(owner, &call.name, call.arguments.clone())
            .await
        {
            Ok(value) => tool_ok(call, serde_json::to_string(&value).unwrap_or_default()),
            Err(error) => tool_error(call, error.to_string()),
        };
        let payload = if result.is_error {
            EventPayload::ToolFailed {
                result: result.clone(),
            }
        } else {
            EventPayload::ToolCompleted {
                result: result.clone(),
            }
        };
        let _ = self.emit(payload, sink);
        result
    }
    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut tools = agent_tool_definitions();
        tools.extend(
            self.extensions
                .tools()
                .into_iter()
                .map(|(_, tool)| ToolDefinition {
                    name: tool.name,
                    description: tool.description,
                    input_schema: tool.input_schema,
                }),
        );
        tools
    }
    /// Emits a `CompletionChanged` event whenever the kernel-derived completion
    /// value actually changes. This is the single announcement point; the model
    /// never writes completion truth.
    fn sync_completion(&mut self, sink: &AgentEventSink) -> Result<()> {
        self.state.recompute_completion(&self.evidence);
        let derived = self.state.state().completion.clone();
        if self.last_completion.as_ref() != Some(&derived) {
            self.last_completion = Some(derived.clone());
            self.emit(
                EventPayload::CompletionChanged {
                    completion: derived,
                },
                sink,
            )?;
        }
        Ok(())
    }
    fn execute_kernel_tool(&mut self, call: &ToolCall, sink: &AgentEventSink) -> ToolResult {
        let started = match self.emit(
            EventPayload::ToolStarted {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            },
            sink,
        ) {
            Ok(event) => event,
            Err(error) => return tool_error(call, error.to_string()),
        };
        let result = match call.name.as_str() {
            "task_update" => match serde_json::from_value::<StateUpdate>(call.arguments.clone()) {
                Ok(update) => {
                    if let Err(error) = self.record_state_memories(&update, started.id) {
                        tool_error(call, error.to_string())
                    } else {
                        self.state.update(update);
                        self.state.recompute_completion(&self.evidence);
                        tool_ok(
                            call,
                            format!(
                                "task state updated\n{}",
                                summarize_state(self.state.state())
                            ),
                        )
                    }
                }
                Err(error) => tool_error(call, format!("invalid task update: {error}")),
            },
            "record_evidence" => {
                let claim = call
                    .arguments
                    .get("claim")
                    .and_then(serde_json::Value::as_str);
                let detail = call
                    .arguments
                    .get("detail")
                    .and_then(serde_json::Value::as_str);
                let status = call
                    .arguments
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_observation_status);
                match (claim, detail, status) {
                    (Some(claim), Some(detail), Some(status)) => {
                        // Only kernel-observed validation can produce Passed or
                        // Failed evidence; the model may declare Pending or
                        // Unavailable observations about non-command claims.
                        let evidence =
                            self.evidence
                                .add(claim, started.id, status, detail);
                        if let Err(error) =
                            self.emit(EventPayload::EvidenceCreated { evidence }, sink)
                        {
                            tool_error(call, error.to_string())
                        } else {
                            self.sync_completion(sink).ok();
                            tool_ok(
                                call,
                                format!(
                                    "evidence recorded for `{claim}`; completion: {:?}",
                                    self.state.state().completion
                                ),
                            )
                        }
                    }
                    (Some(_), _, None) => tool_error(
                        call,
                        "invalid status; record_evidence accepts pending or unavailable. Passed/failed evidence is kernel-owned: run the validate tool".into(),
                    ),
                    _ => tool_error(call, "claim, detail, and status are required".into()),
                }
            }
            "complete" => {
                let implemented = call
                    .arguments
                    .get("implementation_done")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                self.state.set_implementation_done(implemented);
                self.sync_completion(sink).ok();
                tool_ok(
                    call,
                    format!(
                        "implementation claim recorded; kernel-derived completion: {:?}\n{}",
                        self.state.state().completion,
                        summarize_state(self.state.state())
                    ),
                )
            }
            _ => tool_error(call, "unknown kernel tool".into()),
        };
        if call.name == "task_update" || call.name == "complete" {
            let _ = self.emit(
                EventPayload::TaskStateUpdated {
                    state: self.state.state().clone(),
                },
                sink,
            );
        }
        let payload = if result.is_error {
            EventPayload::ToolFailed {
                result: result.clone(),
            }
        } else {
            EventPayload::ToolCompleted {
                result: result.clone(),
            }
        };
        let _ = self.emit(payload, sink);
        result
    }
    /// Kernel-owned validation: the model names a requirement and a command,
    /// the kernel executes it, records the ValidationResult, links the evidence
    /// to real provenance, registers the requirement, and derives completion.
    /// The model never supplies or sees an internal event or call id.
    async fn execute_validate(
        &mut self,
        call: &ToolCall,
        cancel: CancellationToken,
        sink: &AgentEventSink,
    ) -> ToolResult {
        if let Err(error) = self.emit(
            EventPayload::ToolStarted {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            },
            sink,
        ) {
            return tool_error(call, error.to_string());
        }
        let requirement = call
            .arguments
            .get("requirement")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let command = call
            .arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let (Some(requirement), Some(command)) = (requirement, command) else {
            return tool_error(call, "requirement and command are required".into());
        };
        // Validation runs commands, so it obeys the same policy as shell.
        let decision = self.tools.policy_decision("validate", &call.arguments);
        if let crate::tools::PolicyDecision::Deny(reason)
        | crate::tools::PolicyDecision::Ask(reason) = decision
        {
            let _ = self.emit(
                EventPayload::PermissionDecision {
                    tool: call.name.clone(),
                    decision: "policy".into(),
                    reason: reason.clone(),
                },
                sink,
            );
            let denied = tool_error(call, reason);
            let _ = self.emit(
                EventPayload::ToolFailed {
                    result: denied.clone(),
                },
                sink,
            );
            return denied;
        }
        let timeout = call
            .arguments
            .get("timeout_seconds")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(600);
        let output = match self.tools.run_process(&command, timeout, cancel).await {
            Ok(output) => output,
            Err(error) => {
                let failed = tool_error(call, format!("{error:#}"));
                let _ = self.emit(
                    EventPayload::ToolFailed {
                        result: failed.clone(),
                    },
                    sink,
                );
                return failed;
            }
        };
        let passed = output.success;
        let detail = format!(
            "{} ({:.1?}): {}",
            output.status_line,
            output.elapsed,
            output.first_line()
        );
        // 1. Durable ValidationResult…
        let validation_event = match self.emit(
            EventPayload::ValidationResult {
                command: command.clone(),
                passed,
                detail: detail.clone(),
            },
            sink,
        ) {
            Ok(event) => event,
            Err(error) => return tool_error(call, error.to_string()),
        };
        // 2. …linked automatically to evidence for the named requirement.
        let status = if passed {
            EvidenceStatus::Passed
        } else {
            EvidenceStatus::Failed
        };
        let evidence = self
            .evidence
            .add(&requirement, validation_event.id, status, detail);
        if let Err(error) = self.emit(EventPayload::EvidenceCreated { evidence }, sink) {
            return tool_error(call, error.to_string());
        }
        // 3. Kernel bookkeeping: the validated requirement becomes required
        //    and completion is derived.
        self.state.require_validation(&requirement);
        self.sync_completion(sink).ok();
        self.emit(
            EventPayload::TaskStateUpdated {
                state: self.state.state().clone(),
            },
            sink,
        )
        .ok();
        let completion = self.state.state().completion.clone();
        let verdict = if passed { "PASSED" } else { "FAILED" };
        let artifact_note = output
            .artifact_id
            .as_ref()
            .map(|artifact| format!("\n[full output artifact: {artifact}]"))
            .unwrap_or_default();
        // Elapsed time goes last so the head of the body — the failure
        // signature source shared with replayed history — stays deterministic
        // across runs of the same command.
        let mut body = format!(
            "{verdict} requirement `{requirement}`: {command}\n{}\nCompletion: {completion:?}\n",
            output.status_line,
        );
        let preview: String = output.text.chars().take(2000).collect();
        body.push_str(preview.trim_end());
        body.push_str(&format!("\n(elapsed {:.1?})", output.elapsed));
        body.push_str(&artifact_note);
        let result = ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: body,
            is_error: !passed,
            artifact_id: output.artifact_id,
        };
        // 4. The validation's own failure lineage is supervised against the
        //    result body, the exact text a resumed session replays — so live
        //    and restored supervision count identically.
        if passed {
            self.failures.resolve(&requirement);
        } else {
            let decision = self.failures.record(&requirement, &result.output);
            let _ = self.emit(
                EventPayload::FailureAttempt {
                    signature: decision.signature,
                    count: decision.count,
                },
                sink,
            );
            if decision.reground {
                let _ = self.emit(
                    EventPayload::RegroundRequested {
                        signature: requirement.clone(),
                    },
                    sink,
                );
            }
        }
        let payload = if result.is_error {
            EventPayload::ToolFailed {
                result: result.clone(),
            }
        } else {
            EventPayload::ToolCompleted {
                result: result.clone(),
            }
        };
        let _ = self.emit(payload, sink);
        result
    }
    fn record_state_memories(&self, update: &StateUpdate, source: Uuid) -> Result<()> {
        let existing = self.store.memories(self.session_id)?;
        // Model-authored constraints carry TaskConstraint provenance: they are
        // working assumptions from the model, never user instructions.
        let mut records = update
            .add_constraints
            .iter()
            .filter(|text| !self.state.state().constraints.contains(text))
            .map(|text| (MemoryKind::TaskConstraint, text, Validity::Active))
            .chain(
                update
                    .add_decisions
                    .iter()
                    .filter(|text| !self.state.state().decisions.contains(text))
                    .map(|text| (MemoryKind::Decision, text, Validity::Active)),
            )
            .chain(
                update
                    .add_hypotheses
                    .iter()
                    .map(|text| {
                        let validity = if update.reject_hypotheses.contains(text) {
                            Validity::Rejected
                        } else {
                            Validity::Active
                        };
                        (MemoryKind::Hypothesis, text, validity)
                    })
                    .filter(|(_, text, _)| {
                        !self
                            .state
                            .state()
                            .hypotheses
                            .iter()
                            .any(|hypothesis| hypothesis.text.as_str() == text.as_str())
                    }),
            )
            .collect::<Vec<_>>();
        for (kind, text, validity) in records.drain(..) {
            self.store.add_memory(&MemoryRecord {
                id: Uuid::new_v4(),
                session_id: self.session_id,
                kind,
                content: text.clone(),
                originating_event: source,
                created_at: Utc::now(),
                validity,
                confidence: None,
                dependencies: vec![],
                supersedes: None,
            })?;
        }
        let supersede = |kind: MemoryKind, texts: &[String]| -> Result<()> {
            for text in texts {
                if let Some(previous) = existing
                    .iter()
                    .rev()
                    .find(|memory| memory.kind == kind && memory.content == *text)
                {
                    self.store
                        .set_memory_validity(previous.id, Validity::Superseded)?;
                }
            }
            Ok(())
        };
        supersede(MemoryKind::TaskConstraint, &update.supersede_constraints)?;
        supersede(MemoryKind::Decision, &update.supersede_decisions)?;
        for rejected in update
            .reject_hypotheses
            .iter()
            .filter(|text| !update.add_hypotheses.contains(text))
        {
            if let Some(previous) = existing.iter().rev().find(|memory| {
                memory.kind == MemoryKind::Hypothesis
                    && memory.content == **rejected
                    && memory.validity == Validity::Active
            }) {
                self.store
                    .set_memory_validity(previous.id, Validity::Superseded)?;
                self.store.add_memory(&MemoryRecord {
                    id: Uuid::new_v4(),
                    session_id: self.session_id,
                    kind: MemoryKind::Hypothesis,
                    content: rejected.clone(),
                    originating_event: source,
                    created_at: Utc::now(),
                    validity: Validity::Rejected,
                    confidence: None,
                    dependencies: vec![],
                    supersedes: Some(previous.id),
                })?;
            }
        }
        Ok(())
    }
    fn emit(&mut self, payload: EventPayload, sink: &AgentEventSink) -> Result<Event> {
        // Tool execution appends durable events (mutations, drift detection,
        // lifecycle) directly to the store. Deliver those to the live sink
        // first so consumers observe the exact durable order that replay sees.
        self.forward_appended_events(sink)?;
        let event = self.store.append(self.session_id, payload)?;
        sink(AgentOutput::Durable(Box::new(event.clone())));
        self.forward_watermark.set(event.sequence as usize);
        Ok(event)
    }
    /// Forwards durable events appended since the watermark to the live sink.
    /// This keeps live presentation and sidebar state in sync with events that
    /// never pass through [`Self::emit`], such as `FileChanged` or
    /// `ExternalFileChangeDetected`.
    fn forward_appended_events(&self, sink: &AgentEventSink) -> Result<()> {
        let watermark = self.forward_watermark.get();
        let count = self.store.event_count(self.session_id)?;
        if count <= watermark {
            if count < watermark {
                self.forward_watermark.set(count);
            }
            return Ok(());
        }
        let events = self.store.events(self.session_id)?;
        for event in &events[watermark..] {
            sink(AgentOutput::Durable(Box::new(event.clone())));
        }
        self.forward_watermark.set(events.len());
        Ok(())
    }
}

/// Compact human-readable task state for tool results, replacing raw JSON so
/// the model sees a readable summary without internal identifiers.
fn summarize_state(state: &latch_protocol::TaskState) -> String {
    let mut lines = vec![format!("goal: {}", state.goal)];
    if !state.constraints.is_empty() {
        lines.push(format!("constraints: {}", state.constraints.join("; ")));
    }
    if !state.decisions.is_empty() {
        lines.push(format!("decisions: {}", state.decisions.join("; ")));
    }
    let active: Vec<&str> = state
        .hypotheses
        .iter()
        .filter(|h| h.validity == Validity::Active)
        .map(|h| h.text.as_str())
        .collect();
    if !active.is_empty() {
        lines.push(format!("hypotheses: {}", active.join("; ")));
    }
    if !state.rejected_hypotheses.is_empty() {
        lines.push(format!(
            "rejected hypotheses: {}",
            state.rejected_hypotheses.join("; ")
        ));
    }
    if !state.required_validations.is_empty() {
        lines.push(format!(
            "required validations: {}",
            state.required_validations.join("; ")
        ));
    }
    format!("completion: {:?}\n{}", state.completion, lines.join("\n"))
}

fn agent_tool_definitions() -> Vec<ToolDefinition> {
    let mut tools = ToolExecutor::definitions();
    tools.extend([
        ToolDefinition{name:"validate".into(),description:"Run a validation command for a named requirement. The kernel executes it, records the result as evidence linked to real provenance, and derives completion. You never supply event or call identifiers — pass a semantic requirement name and the command that proves it. A requirement that already failed and now passes supersedes the old result.".into(),input_schema:json!({"type":"object","required":["requirement","command"],"properties":{"requirement":{"type":"string","description":"Semantic name of the requirement, e.g. 'existing unittest passes'"},"command":{"type":"string"},"timeout_seconds":{"type":"integer"}}})},
        ToolDefinition{name:"task_update".into(),description:"Propose an update to canonical task state. Constraints you add are TaskConstraints (working rules you propose), not user constraints. Use supersede fields to replace outdated decisions/constraints and resolve_questions to close answered questions.".into(),input_schema:json!({"type":"object","properties":{"goal":{"type":["string","null"]},"add_constraints":{"type":"array","items":{"type":"string"}},"supersede_constraints":{"type":"array","items":{"type":"string"}},"add_decisions":{"type":"array","items":{"type":"string"}},"supersede_decisions":{"type":"array","items":{"type":"string"}},"add_hypotheses":{"type":"array","items":{"type":"string"}},"reject_hypotheses":{"type":"array","items":{"type":"string"}},"touched_files":{"type":"array","items":{"type":"string"}},"required_validations":{"type":"array","items":{"type":"string"},"description":"Requirements that must hold; their pass state is kernel evidence, not settable here"},"open_questions":{"type":["array","null"],"items":{"type":"string"}},"resolve_questions":{"type":"array","items":{"type":"string"}},"next_actions":{"type":["array","null"],"items":{"type":"string"}},"completion_criteria":{"type":"array","items":{"type":"string"}}}})},
        ToolDefinition{name:"record_evidence".into(),description:"Record an observation for a non-command claim. Only pending and unavailable statuses are accepted; passed/failed evidence is kernel-owned and comes from the validate tool.".into(),input_schema:json!({"type":"object","required":["claim","status","detail"],"properties":{"claim":{"type":"string"},"status":{"enum":["pending","unavailable"]},"detail":{"type":"string"}}})},
        ToolDefinition{name:"complete".into(),description:"State that implementation work is done. The kernel derives completion (Verified / ImplementedNotVerified / Blocked / InProgress) from this claim plus current validation evidence.".into(),input_schema:json!({"type":"object","required":["implementation_done"],"properties":{"implementation_done":{"type":"boolean"}}})}
    ]);
    tools
}
fn parse_observation_status(value: &str) -> Option<EvidenceStatus> {
    match value {
        "pending" => Some(EvidenceStatus::Pending),
        "unavailable" => Some(EvidenceStatus::Unavailable),
        _ => None,
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
fn tool_ok(call: &ToolCall, output: String) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        output,
        is_error: false,
        artifact_id: None,
    }
}
fn tool_error(call: &ToolCall, output: String) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        output,
        is_error: true,
        artifact_id: None,
    }
}
/// Deterministic provider-valid anchor used when the recent window no longer
/// contains the original user prompt. Canonical state carries the actual task,
/// so this only restores conversational continuity.
const CONTINUATION_ANCHOR: &str = "Kernel: the original user prompt has scrolled out of the active recent window; the canonical task state above remains authoritative. The transcript below continues the current task.";

fn context_messages(ctx: &crate::continuity::MaterializedContext) -> Vec<ModelMessage> {
    let raw = ctx
        .recent
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::UserMessage { text } => Some(ModelMessage::text("user", text.clone())),
            EventPayload::AssistantMessageCompleted {
                text,
                tool_calls,
                reasoning_content,
            } => Some(ModelMessage {
                role: "assistant".into(),
                content: text.clone(),
                tool_calls: tool_calls.clone(),
                tool_call_id: None,
                reasoning_content: reasoning_content.clone(),
            }),
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                Some(ModelMessage {
                    role: "tool".into(),
                    content: result.output.clone(),
                    tool_calls: vec![],
                    tool_call_id: Some(result.call_id.clone()),
                    reasoning_content: None,
                })
            }
            EventPayload::RegroundRequested { signature } => Some(ModelMessage::text("user", format!("Kernel re-ground required after repeated failure {signature}. Re-read current reality, identify disproven assumptions, and form a materially different strategy before another mutation."))),
            EventPayload::ScopeExpansionRequested { mutations, reason } => Some(ModelMessage::text("user", format!("Kernel scope review after {mutations} mutations: {reason}"))),
            _ => None,
        })
        .collect::<Vec<_>>();
    let sanitized = sanitize_tool_history(raw);
    let mut normalized: Vec<ModelMessage> = Vec::new();
    // Once the original user prompt ages out of the recent byte budget, the
    // window legitimately begins mid-task. Providers still need the first
    // non-system message to be a user turn, so anchor the window with a
    // deterministic kernel continuation note. Never drop the transcript: doing
    // so gives the model amnesia and restarts inspection loops.
    if sanitized
        .first()
        .is_some_and(|message| message.role != "user")
    {
        normalized.push(ModelMessage::text("user", CONTINUATION_ANCHOR));
    }
    for message in sanitized {
        if let Some(previous) = normalized.last_mut()
            && previous.role == message.role
            && previous.tool_calls.is_empty()
            && previous.tool_call_id.is_none()
            && message.tool_calls.is_empty()
            && message.tool_call_id.is_none()
        {
            previous.content.push_str("\n\n");
            previous.content.push_str(&message.content);
        } else {
            normalized.push(message);
        }
    }
    normalized
}

/// Enforces structurally valid tool history before it reaches a provider.
///
/// An assistant message proposing tool calls is only kept if every proposed
/// call is answered by a following `tool` message; otherwise the calls are
/// stripped so a provider can never observe a dangling assistant tool call.
/// Tool messages that do not belong to the immediately preceding assistant
/// tool-call turn are dropped so a provider can never observe a dangling tool
/// result.
fn sanitize_tool_history(messages: Vec<ModelMessage>) -> Vec<ModelMessage> {
    let mut sanitized = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.role == "assistant" && !message.tool_calls.is_empty() {
            let expected = message
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let mut end = index + 1;
            while end < messages.len() && messages[end].role == "tool" {
                end += 1;
            }
            let available = messages[index + 1..end]
                .iter()
                .filter_map(|tool| tool.tool_call_id.as_deref())
                .collect::<std::collections::BTreeSet<_>>();
            if expected.is_subset(&available) {
                sanitized.push(message.clone());
                for tool in &messages[index + 1..end] {
                    if tool
                        .tool_call_id
                        .as_deref()
                        .is_some_and(|id| expected.contains(id))
                    {
                        sanitized.push(tool.clone());
                    }
                }
            } else {
                let mut stripped = message.clone();
                stripped.tool_calls.clear();
                sanitized.push(stripped);
            }
            index = end;
        } else if message.role == "tool" {
            index += 1;
        } else {
            sanitized.push(message.clone());
            index += 1;
        }
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ContextConfig, PermissionConfig},
        provider::FakeProvider,
        tools::PolicyEngine,
    };
    use latch_protocol::{ModelResponse, ToolCall};
    use serde_json::json;
    use tempfile::tempdir;
    #[tokio::test]
    async fn loop_executes_multiple_read_tools() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("a"), "hello").unwrap();
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(d.path()).unwrap();
        let responses = vec![
            ModelResponse {
                text: "checking".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path":"a"}),
                    },
                    ToolCall {
                        id: "2".into(),
                        name: "search".into(),
                        arguments: json!({"query":"hello"}),
                    },
                ],
                stop_reason: "tool_calls".into(),
                usage: None,
                reasoning_content: None,
            },
            ModelResponse {
                text: "done".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ];
        let p = Arc::new(FakeProvider::scripted(responses));
        let policy = PolicyEngine::new(Mode::Ask, d.path().into(), PermissionConfig::default());
        let tools = ToolExecutor::new(
            d.path().into(),
            d.path().join("art"),
            store.clone(),
            sid,
            policy,
        )
        .unwrap();
        let continuity = ContinuityEngine::new(store.clone(), ContextConfig::default());
        let mut a = Agent::new(AgentRuntime {
            session_id: sid,
            workspace: d.path().into(),
            mode: Mode::Ask,
            store: store.clone(),
            provider: p,
            tools,
            continuity,
            retry_budget: 2,
        });
        let out = a
            .run("inspect", CancellationToken::new(), Arc::new(|_| {}))
            .await
            .unwrap();
        assert_eq!(out, "checkingdone");
        let events = store.events(sid).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.payload, EventPayload::ToolCompleted { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn loop_executes_registered_extension_tool() {
        let d = tempdir().unwrap();
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(d.path()).unwrap();
        let provider = Arc::new(FakeProvider::scripted(vec![
            ModelResponse {
                text: "calling extension".into(),
                tool_calls: vec![ToolCall {
                    id: "ext-1".into(),
                    name: "fixture.echo".into(),
                    arguments: json!({"value":"through-agent"}),
                }],
                stop_reason: "tool_calls".into(),
                usage: None,
                reasoning_content: None,
            },
            ModelResponse {
                text: "extension complete".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolExecutor::new(
            d.path().into(),
            d.path().join("art"),
            store.clone(),
            sid,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        let mut agent = Agent::new(AgentRuntime {
            session_id: sid,
            workspace: d.path().into(),
            mode: Mode::Work,
            store: store.clone(),
            provider,
            tools,
            continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
            retry_budget: 2,
        });
        let fixture = format!("{}/tests/fixtures/extension.py", env!("CARGO_MANIFEST_DIR"));
        agent
            .load_extension("fixture".into(), "python3", &[fixture])
            .await
            .unwrap();
        agent
            .run(
                "use the extension",
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
        assert!(store.events(sid).unwrap().iter().any(|event| matches!(&event.payload, EventPayload::ToolCompleted { result } if result.name == "fixture.echo" && result.output.contains("through-agent"))));
        agent.shutdown_extensions().await.unwrap();
    }

    #[tokio::test]
    async fn validate_links_kernel_provenance_without_model_ids() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("x.txt"), "good").unwrap();
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(d.path()).unwrap();
        let provider = Arc::new(FakeProvider::scripted(vec![
            ModelResponse {
                text: "validating".into(),
                tool_calls: vec![ToolCall {
                    id: "model-call-1".into(),
                    name: "validate".into(),
                    arguments: json!({
                        "requirement": "content is good",
                        "command": "test \"$(cat x.txt)\" = good"
                    }),
                }],
                stop_reason: "tool_calls".into(),
                usage: None,
                reasoning_content: None,
            },
            ModelResponse {
                text: "validated".into(),
                tool_calls: vec![ToolCall {
                    id: "model-call-2".into(),
                    name: "complete".into(),
                    arguments: json!({"implementation_done": true}),
                }],
                stop_reason: "tool_calls".into(),
                usage: None,
                reasoning_content: None,
            },
            ModelResponse {
                text: "done".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolExecutor::new(
            d.path().into(),
            d.path().join("art"),
            store.clone(),
            sid,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        let mut agent = Agent::new(AgentRuntime {
            session_id: sid,
            workspace: d.path().into(),
            mode: Mode::Work,
            store: store.clone(),
            provider,
            tools,
            continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
            retry_budget: 3,
        });
        agent
            .run("verify it", CancellationToken::new(), Arc::new(|_| {}))
            .await
            .unwrap();
        let events = store.events(sid).unwrap();
        // Kernel recorded a ValidationResult and evidence with real provenance.
        let validation = events
            .iter()
            .find_map(|e| match &e.payload {
                EventPayload::ValidationResult {
                    command, passed, ..
                } => Some((command.clone(), *passed)),
                _ => None,
            })
            .expect("validation result recorded");
        assert_eq!(validation, ("test \"$(cat x.txt)\" = good".into(), true));
        let evidence = events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::EvidenceCreated { evidence } => Some(evidence.clone()),
                _ => None,
            })
            .next_back()
            .expect("evidence created");
        assert_eq!(evidence.status, EvidenceStatus::Passed);
        assert_eq!(evidence.claim, "content is good");
        // The evidence source points at a real durable event (the validation
        // result), not at a model-supplied id.
        assert!(events.iter().any(|e| e.id == evidence.source_event
            && matches!(&e.payload, EventPayload::ValidationResult { .. })));
        // The requirement was kernel-registered and completion derived.
        assert!(
            agent
                .state()
                .required_validations
                .iter()
                .any(|r| r == "content is good")
        );
        assert_eq!(agent.state().completion, CompletionState::Verified);
    }

    #[tokio::test]
    async fn record_evidence_rejects_kernel_owned_statuses() {
        let d = tempdir().unwrap();
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(d.path()).unwrap();
        let provider = Arc::new(FakeProvider::scripted(vec![
            ModelResponse {
                text: "claiming".into(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "record_evidence".into(),
                    arguments: json!({"claim":"tests pass","status":"passed","detail":"self-asserted"}),
                }],
                stop_reason: "tool_calls".into(),
                usage: None,
                reasoning_content: None,
            },
            ModelResponse {
                text: "done".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolExecutor::new(
            d.path().into(),
            d.path().join("art"),
            store.clone(),
            sid,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        let mut agent = Agent::new(AgentRuntime {
            session_id: sid,
            workspace: d.path().into(),
            mode: Mode::Work,
            store: store.clone(),
            provider,
            tools,
            continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
            retry_budget: 3,
        });
        agent
            .run("try it", CancellationToken::new(), Arc::new(|_| {}))
            .await
            .unwrap();
        // The self-passed evidence was refused: no Passed evidence exists.
        assert!(
            !store
                .events(sid)
                .unwrap()
                .iter()
                .any(|e| matches!(&e.payload, EventPayload::EvidenceCreated { evidence } if evidence.status == EvidenceStatus::Passed))
        );
    }

    #[tokio::test]
    async fn fail_then_pass_validation_supersedes_completion() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("x.txt"), "bad").unwrap();
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(d.path()).unwrap();
        let tools = ToolExecutor::new(
            d.path().into(),
            d.path().join("art"),
            store.clone(),
            sid,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        let mut agent = Agent::new(AgentRuntime {
            session_id: sid,
            workspace: d.path().into(),
            mode: Mode::Work,
            store: store.clone(),
            provider: Arc::new(FakeProvider::scripted(vec![])),
            tools,
            continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
            retry_budget: 3,
        });
        // Baseline validation fails; the implementation claim is not yet made.
        let failed = agent
            .run_validation(
                "content good",
                "test \"$(cat x.txt)\" = good",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(failed.is_error);
        agent.state.set_implementation_done(true);
        agent
            .run_validation(
                "content good",
                "test \"$(cat x.txt)\" = good",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            agent.state().completion,
            CompletionState::ImplementedNotVerified
        );
        // Fix, then revalidate: the PASS supersedes the earlier FAIL.
        std::fs::write(d.path().join("x.txt"), "good").unwrap();
        let passed = agent
            .run_validation(
                "content good",
                "test \"$(cat x.txt)\" = good",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!passed.is_error);
        assert_eq!(agent.state().completion, CompletionState::Verified);
        // All raw attempts remain in history, and the failing ones stay Failed
        // while current evidence for the claim is Passed.
        let events = store.events(sid).unwrap();
        let validation_results = events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::ValidationResult { .. }))
            .count();
        assert_eq!(validation_results, 3);
        assert_eq!(
            agent.evidence.status_of("content good"),
            Some(EvidenceStatus::Passed)
        );
        let failed_entries = agent
            .evidence
            .entries()
            .iter()
            .filter(|e| e.status == EvidenceStatus::Failed)
            .count();
        assert_eq!(failed_entries, 2, "history keeps the failed attempts");
        // A passing validation resolved its failure lineage.
        assert!(agent.failure_lineages().is_empty());
    }

    #[test]
    fn sanitizer_keeps_reasoning_on_corrupt_history_and_whole_transactions() {
        // A genuinely corrupt old session: assistant proposes two calls but
        // only one result exists. The sanitizer must still keep reasoning so a
        // thinking provider never loses required state, even though the
        // dangling tool call is stripped.
        let corrupt = vec![
            ModelMessage::text("user", "inspect"),
            ModelMessage {
                role: "assistant".into(),
                content: "thinking".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "a".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "a"}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "b"}),
                    },
                ],
                tool_call_id: None,
                reasoning_content: Some("reasoned".into()),
            },
            ModelMessage {
                role: "tool".into(),
                content: "b result".into(),
                tool_calls: vec![],
                tool_call_id: Some("b".into()),
                reasoning_content: None,
            },
        ];
        let sanitized = sanitize_tool_history(corrupt);
        let assistant = sanitized
            .iter()
            .find(|m| m.role == "assistant")
            .expect("assistant kept");
        assert!(assistant.tool_calls.is_empty(), "dangling calls stripped");
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("reasoned"),
            "reasoning survives the defensive transform"
        );
        // With the lifecycle invariant, complete transactions (denied call
        // included) are kept whole: reasoning + both tool_calls + all results.
        let complete = vec![
            ModelMessage::text("user", "inspect"),
            ModelMessage {
                role: "assistant".into(),
                content: "thinking".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "a".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "a"}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "b"}),
                    },
                ],
                tool_call_id: None,
                reasoning_content: Some("reasoned".into()),
            },
            ModelMessage {
                role: "tool".into(),
                content: "a denied".into(),
                tool_calls: vec![],
                tool_call_id: Some("a".into()),
                reasoning_content: None,
            },
            ModelMessage {
                role: "tool".into(),
                content: "b result".into(),
                tool_calls: vec![],
                tool_call_id: Some("b".into()),
                reasoning_content: None,
            },
        ];
        let kept = sanitize_tool_history(complete);
        let assistant = kept
            .iter()
            .find(|m| m.role == "assistant")
            .expect("assistant kept");
        assert_eq!(assistant.tool_calls.len(), 2);
        assert_eq!(assistant.reasoning_content.as_deref(), Some("reasoned"));
        assert_eq!(
            kept.iter().filter(|m| m.role == "tool").count(),
            2,
            "both results kept"
        );
    }

    #[test]
    fn context_messages_anchor_mid_task_windows_instead_of_dropping_them() {
        fn event(sequence: u64, payload: EventPayload) -> Event {
            Event {
                id: Uuid::new_v4(),
                session_id: Uuid::nil(),
                sequence,
                timestamp: Utc::now(),
                parent_id: None,
                payload,
            }
        }
        let assistant = event(
            2,
            EventPayload::AssistantMessageCompleted {
                text: "checking".into(),
                tool_calls: vec![ToolCall {
                    id: "a".into(),
                    name: "read_file".into(),
                    arguments: json!({"path":"a"}),
                }],
                reasoning_content: None,
            },
        );
        let tool = event(
            3,
            EventPayload::ToolCompleted {
                result: ToolResult {
                    call_id: "a".into(),
                    name: "read_file".into(),
                    output: "hash: x\ncontents".into(),
                    is_error: false,
                    artifact_id: None,
                },
            },
        );
        let base = crate::continuity::MaterializedContext {
            system: "system".into(),
            canonical: String::new(),
            recalled: String::new(),
            recent: vec![assistant.clone(), tool.clone()],
            bridge: crate::continuity::ConversationBridge::default(),
            episodes: vec![],
            stats: latch_protocol::ContextStats::default(),
        };
        // The user prompt already scrolled out of the window: the transcript is
        // preserved behind a deterministic kernel continuation anchor.
        let messages = context_messages(&base);
        assert_eq!(messages.first().map(|m| m.role.as_str()), Some("user"));
        assert!(
            messages[0]
                .content
                .contains("scrolled out of the active recent window")
        );
        assert!(
            messages
                .iter()
                .any(|m| m.role == "assistant" && !m.tool_calls.is_empty())
        );
        assert!(
            messages
                .iter()
                .any(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("a"))
        );
        // A window that still holds the user prompt is replayed verbatim.
        let with_user = crate::continuity::MaterializedContext {
            recent: vec![
                event(
                    1,
                    EventPayload::UserMessage {
                        text: "do it".into(),
                    },
                ),
                assistant,
                tool,
            ],
            ..base
        };
        let messages = context_messages(&with_user);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "do it");
    }
}
