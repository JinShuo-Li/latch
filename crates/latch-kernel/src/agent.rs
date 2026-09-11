use crate::continuity::ContinuityEngine;
use crate::extension::ExtensionRegistry;
use crate::prompt::PromptCompiler;
use crate::provider::{ModelProvider, StreamSink};
use crate::state::{EvidenceLedger, FailureManager, StateUpdate, TaskStateManager};
use crate::store::EventStore;
use crate::tools::ToolExecutor;
use anyhow::{Result, anyhow};
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
}
impl Agent {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        session_id: Uuid,
        workspace: PathBuf,
        mode: Mode,
        store: EventStore,
        provider: Arc<dyn ModelProvider>,
        tools: ToolExecutor,
        continuity: ContinuityEngine,
        retry_budget: u32,
    ) -> Self {
        Self {
            session_id,
            workspace,
            mode,
            store,
            provider,
            tools,
            continuity,
            state: TaskStateManager::default(),
            evidence: EvidenceLedger::default(),
            extensions: ExtensionRegistry::new(),
            failures: FailureManager::new(retry_budget),
            max_model_retries: 2,
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
        self.emit(
            EventPayload::UserMessage {
                text: user_text.into(),
            },
            &sink,
        )?;
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
        let mut tool_results: Vec<ToolResult> = vec![];
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
            let mut messages = context_messages(&ctx);
            for result in &tool_results {
                messages.push(ModelMessage {
                    role: "user".into(),
                    content: format!(
                        "Tool result for {} ({}):\n{}",
                        result.name, result.call_id, result.output
                    ),
                });
            }
            let request = ModelRequest {
                system: format!(
                    "{}\n\n{}\n\nRECALLED ORIGINAL MATERIAL\n{}",
                    ctx.system, ctx.canonical, ctx.recalled
                ),
                messages,
                tools: self.tool_definitions(),
            };
            self.emit(
                EventPayload::ModelRequestStarted {
                    provider: self.provider.name().into(),
                    model: self.provider.model().into(),
                },
                &sink,
            )?;
            let transient = sink.clone();
            let provider_sink: StreamSink = Arc::new(move |e| transient(AgentOutput::Transient(e)));
            let response = self
                .call_with_retry(request, cancel.clone(), provider_sink)
                .await?;
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
            tool_results = self
                .execute_batch(response.tool_calls, cancel.clone(), &sink)
                .await;
            if self.tools.latch_change_count().await > 12 {
                tool_results.push(ToolResult { call_id: "kernel-scope".into(), name: "scope_budget".into(), output: "The change now spans more than 12 Latch mutations. Explain why this expansion is required by the user task before continuing.".into(), is_error: false, artifact_id: None });
            }
            let mut reground = None;
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
                        reground = Some(d.signature);
                    }
                } else {
                    self.failures.improvement();
                }
            }
            if let Some(signature) = reground {
                tool_results.push(ToolResult{call_id:"kernel-reground".into(),name:"re_ground".into(),output:format!("Repeated failure {signature}. Re-read current reality, identify disproven assumptions, and form a materially different strategy before another mutation."),is_error:false,artifact_id:None});
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
        if calls
            .iter()
            .all(|c| matches!(c.name.as_str(), "read_file" | "search" | "git_status"))
        {
            let tasks = calls
                .into_iter()
                .map(|call| {
                    let tools = self.tools.clone();
                    let c = cancel.clone();
                    tokio::spawn(async move { tools.execute(&call, c).await })
                })
                .collect::<Vec<_>>();
            let mut results = Vec::new();
            for task in tasks {
                match task.await {
                    Ok(r) => results.push(r),
                    Err(e) => results.push(ToolResult {
                        call_id: "join".into(),
                        name: "scheduler".into(),
                        output: e.to_string(),
                        is_error: true,
                        artifact_id: None,
                    }),
                }
            }
            results
        } else {
            let mut results = Vec::new();
            for call in calls {
                if matches!(
                    call.name.as_str(),
                    "task_update" | "record_evidence" | "complete"
                ) {
                    results.push(self.execute_kernel_tool(&call, sink));
                } else if let Some(owner) = self.extensions.owner_for_tool(&call.name) {
                    results.push(self.execute_extension_tool(&owner, &call, sink).await);
                } else {
                    results.push(self.tools.execute(&call, cancel.clone()).await);
                }
            }
            results
        }
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
                        let evidence = self.evidence.add(claim, started.id, status, detail);
                        if let Err(error) =
                            self.emit(EventPayload::EvidenceCreated { evidence }, sink)
                        {
                            tool_error(call, error.to_string())
                        } else {
                            tool_ok(call, "evidence recorded".into())
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
        let records = update
            .add_constraints
            .iter()
            .map(|text| (MemoryKind::UserConstraint, text, Validity::Active))
            .chain(
                update
                    .add_decisions
                    .iter()
                    .map(|text| (MemoryKind::Decision, text, Validity::Active)),
            )
            .chain(update.add_hypotheses.iter().map(|text| {
                let validity = if update.reject_hypotheses.contains(text) {
                    Validity::Rejected
                } else {
                    Validity::Active
                };
                (MemoryKind::Hypothesis, text, validity)
            }));
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
        Ok(())
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
 ToolDefinition{name:"record_evidence".into(),description:"Record provenance-linked evidence for a meaningful claim.".into(),input_schema:json!({"type":"object","required":["claim","status","detail"],"properties":{"claim":{"type":"string"},"status":{"enum":["pending","passed","failed","unavailable"]},"detail":{"type":"string"}}})},
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
    ctx.recent
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
            _ => None,
        })
        .collect()
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
        let mut a = Agent::new(
            sid,
            d.path().into(),
            Mode::Ask,
            store.clone(),
            p,
            tools,
            continuity,
            2,
        );
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
}
