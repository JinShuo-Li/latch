use crate::continuity::ContinuityEngine;
use crate::extension::{ExtensionGuardDecision, ExtensionRegistry};
use crate::prompt::PromptCompiler;
use crate::provider::{ModelProvider, StreamSink};
use crate::state::{EvidenceLedger, FailureManager, StateUpdate, TaskStateManager};
use crate::store::EventStore;
use crate::tools::ToolExecutor;
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use latch_protocol::{
    Event, EventPayload, EvidenceStatus, MemoryKind, MemoryRecord, Mode, ModelMessage,
    ModelRequest, StreamEvent, ToolCall, ToolDefinition, ToolResult, Validity,
};
use serde_json::json;
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
    max_model_retries: u32,
    scope_warned: bool,
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
            max_model_retries: 2,
            scope_warned: false,
        }
    }
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.tools.set_mode(mode);
    }
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }
    #[must_use]
    pub fn state(&self) -> &latch_protocol::TaskState {
        self.state.state()
    }
    pub fn restore_state(&mut self, state: latch_protocol::TaskState) {
        self.state = TaskStateManager::new(state);
    }
    pub fn restore_evidence(&mut self, evidence: Vec<latch_protocol::Evidence>) {
        self.evidence = EvidenceLedger::new(evidence);
    }
    pub fn context(&self, query: Option<&str>) -> Result<crate::continuity::MaterializedContext> {
        let prompt = PromptCompiler::compile(self.mode, self.state.state(), &self.workspace)?;
        self.continuity
            .materialize(self.session_id, self.state.state(), query, prompt.text)
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
    pub async fn builtin_tool(&self, name: &str, cancel: CancellationToken) -> ToolResult {
        self.tools
            .execute(
                &latch_protocol::ToolCall {
                    id: format!("builtin-{}", Uuid::new_v4()),
                    name: name.into(),
                    arguments: serde_json::json!({}),
                },
                cancel,
            )
            .await
    }
    pub async fn run(
        &mut self,
        user_text: &str,
        cancel: CancellationToken,
        sink: AgentEventSink,
    ) -> Result<String> {
        let user_event = self.emit(
            EventPayload::UserMessage {
                text: user_text.into(),
            },
            &sink,
        )?;
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
            let e = self.emit(
                EventPayload::TaskStateUpdated {
                    state: self.state.state().clone(),
                },
                &sink,
            )?;
            let _ = e;
        }
        let mut final_text = String::new();
        let mut turns = 0u32;
        loop {
            turns += 1;
            if turns > 32 {
                return Err(anyhow!("agent exceeded 32 tool turns"));
            }
            let query = if turns == 1 { Some(user_text) } else { None };
            let ctx = self.context(query)?;
            let event = self.emit(
                EventPayload::ContextMaterialized {
                    stats: ctx.stats.clone(),
                },
                &sink,
            )?;
            let _ = event;
            let messages = context_messages(&ctx);
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
            let tool_results = self
                .execute_batch(response.tool_calls, cancel.clone(), &sink)
                .await;
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
            for r in &tool_results {
                if r.is_error {
                    let d = self.failures.record(&r.name, &r.output);
                    self.emit(
                        EventPayload::FailureAttempt {
                            signature: d.signature.clone(),
                            count: d.count,
                        },
                        &sink,
                    )?;
                    if d.reground {
                        self.emit(
                            EventPayload::RegroundRequested {
                                signature: d.signature.clone(),
                            },
                            &sink,
                        )?;
                    }
                } else {
                    self.failures.improvement();
                }
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
                Err(error) => results.push(tool_error(
                    &call,
                    format!("extension guard failed: {error}"),
                )),
            }
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
                        tool_ok(
                            call,
                            serde_json::to_string(self.state.state()).unwrap_or_default(),
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
                    .and_then(parse_evidence_status);
                match (claim, detail, status) {
                    (Some(claim), Some(detail), Some(status)) => {
                        match self.evidence_source(&status, call, started.id) {
                            Ok(source) => {
                                let evidence = self.evidence.add(claim, source, status, detail);
                                if let Err(error) =
                                    self.emit(EventPayload::EvidenceCreated { evidence }, sink)
                                {
                                    tool_error(call, error.to_string())
                                } else {
                                    tool_ok(call, "evidence recorded".into())
                                }
                            }
                            Err(error) => tool_error(call, error.to_string()),
                        }
                    }
                    _ => tool_error(call, "claim, detail, and valid status are required".into()),
                }
            }
            "complete" => {
                let implemented = call
                    .arguments
                    .get("implementation_done")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                self.state.recompute_completion(implemented, &self.evidence);
                tool_ok(
                    call,
                    format!("completion: {:?}", self.state.state().completion),
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
    fn record_state_memories(&self, update: &StateUpdate, source: Uuid) -> Result<()> {
        let existing = self.store.memories(self.session_id)?;
        let records = update
            .add_constraints
            .iter()
            .filter(|text| !self.state.state().constraints.contains(text))
            .map(|text| (MemoryKind::UserConstraint, text, Validity::Active))
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
            );
        for (kind, text, validity) in records {
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
                    dependencies: vec![previous.id],
                    supersedes: Some(previous.id),
                })?;
            }
        }
        Ok(())
    }
    fn evidence_source(
        &self,
        status: &EvidenceStatus,
        call: &ToolCall,
        fallback: Uuid,
    ) -> Result<Uuid> {
        if matches!(
            status,
            EvidenceStatus::Pending | EvidenceStatus::Unavailable
        ) {
            return Ok(fallback);
        }
        let source_call = call
            .arguments
            .get("source_call_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("passed or failed evidence requires source_call_id"))?;
        self.store
            .events(self.session_id)?
            .into_iter()
            .rev()
            .find_map(|event| match (&event.payload, status) {
                (EventPayload::ToolCompleted { result }, EvidenceStatus::Passed)
                    if result.call_id == source_call =>
                {
                    Some(event.id)
                }
                (EventPayload::ToolFailed { result }, EvidenceStatus::Failed)
                    if result.call_id == source_call =>
                {
                    Some(event.id)
                }
                _ => None,
            })
            .ok_or_else(|| {
                anyhow!("source_call_id does not reference matching observed tool evidence")
            })
    }
    fn emit(&self, payload: EventPayload, sink: &AgentEventSink) -> Result<Event> {
        let event = self.store.append(self.session_id, payload)?;
        sink(AgentOutput::Durable(Box::new(event.clone())));
        Ok(event)
    }
}
fn agent_tool_definitions() -> Vec<ToolDefinition> {
    let mut tools = ToolExecutor::definitions();
    tools.extend([
 ToolDefinition{name:"task_update".into(),description:"Propose a validated additive update to canonical task state.".into(),input_schema:json!({"type":"object","properties":{"goal":{"type":["string","null"]},"add_constraints":{"type":"array","items":{"type":"string"}},"add_decisions":{"type":"array","items":{"type":"string"}},"add_hypotheses":{"type":"array","items":{"type":"string"}},"reject_hypotheses":{"type":"array","items":{"type":"string"}},"touched_files":{"type":"array","items":{"type":"string"}},"required_validations":{"type":"array","items":{"type":"string"}},"validation_status":{"type":"object","additionalProperties":{"type":"boolean"}},"open_questions":{"type":["array","null"],"items":{"type":"string"}},"next_actions":{"type":["array","null"],"items":{"type":"string"}},"completion_criteria":{"type":"array","items":{"type":"string"}}}})},
 ToolDefinition{name:"record_evidence".into(),description:"Record provenance-linked evidence. Passed/failed evidence requires source_call_id for a matching completed/failed tool.".into(),input_schema:json!({"type":"object","required":["claim","status","detail"],"properties":{"claim":{"type":"string"},"status":{"enum":["pending","passed","failed","unavailable"]},"detail":{"type":"string"},"source_call_id":{"type":"string"}}})},
 ToolDefinition{name:"complete".into(),description:"Ask the kernel to calculate completion from implementation and evidence state.".into(),input_schema:json!({"type":"object","required":["implementation_done"],"properties":{"implementation_done":{"type":"boolean"}}})}
]);
    tools
}
fn parse_evidence_status(value: &str) -> Option<EvidenceStatus> {
    match value {
        "pending" => Some(EvidenceStatus::Pending),
        "passed" => Some(EvidenceStatus::Passed),
        "failed" => Some(EvidenceStatus::Failed),
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
fn context_messages(ctx: &crate::continuity::MaterializedContext) -> Vec<ModelMessage> {
    let raw = ctx.recent
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::UserMessage { text } => Some(ModelMessage {
                role: "user".into(),
                content: text.clone(),
            }),
            EventPayload::AssistantMessageCompleted { text, .. } => Some(ModelMessage {
                role: "assistant".into(),
                content: text.clone(),
            }),
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                Some(ModelMessage {
                    role: "user".into(),
                    content: format!("Tool {}: {}", result.name, result.output),
                })
            }
            EventPayload::RegroundRequested { signature } => Some(ModelMessage { role: "user".into(), content: format!("Kernel re-ground required after repeated failure {signature}. Re-read current reality, identify disproven assumptions, and form a materially different strategy before another mutation.") }),
            EventPayload::ScopeExpansionRequested { mutations, reason } => Some(ModelMessage { role: "user".into(), content: format!("Kernel scope review after {mutations} mutations: {reason}") }),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut normalized: Vec<ModelMessage> = Vec::new();
    for message in raw.into_iter().skip_while(|message| message.role != "user") {
        if let Some(previous) = normalized.last_mut()
            && previous.role == message.role
        {
            previous.content.push_str("\n\n");
            previous.content.push_str(&message.content);
        } else {
            normalized.push(message);
        }
    }
    normalized
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
            },
            ModelResponse {
                text: "done".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
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
            },
            ModelResponse {
                text: "extension complete".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
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
}
