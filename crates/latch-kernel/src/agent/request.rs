//! Provider-facing request construction: prompt, tool schemas, canonical state,
//! and the volatile kernel-context tail.

use super::*;

/// Provider-valid anchor used when the recent window no longer contains the
/// original user prompt. Canonical state carries the actual task, so this only
/// restores conversational continuity.
const CONTINUATION_ANCHOR: &str = "Kernel: the original user prompt has scrolled out of the active recent window; the canonical task state above remains authoritative. The transcript below continues the current task — keep working until it is complete or you are blocked on something only the user can resolve.";

impl Agent {
    /// Token budget for the complete request, before tool/extension costs are
    /// known.
    #[must_use]
    pub(super) fn materialize_budget(&self, reserved_tokens: usize) -> MaterializeBudget {
        self.continuity
            .default_budget(self.context_window_tokens, reserved_tokens)
    }

    pub(crate) fn tool_definitions(&self) -> Vec<ToolDefinition> {
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
}

fn agent_tool_definitions() -> Vec<ToolDefinition> {
    let mut tools = ToolExecutor::definitions();
    tools.extend([
        ToolDefinition{name:"validate".into(),description:"Run a validation command for a named requirement. The kernel executes it, records the result as evidence linked to real provenance, and derives completion. You never supply event or call identifiers — pass a semantic requirement name and the command that proves it. A requirement that already failed and now passes supersedes the old result.".into(),input_schema:json!({"type":"object","required":["requirement","command"],"properties":{"requirement":{"type":"string","description":"Semantic name of the requirement, e.g. 'existing unittest passes'"},"command":{"type":"string"},"timeout_seconds":{"type":"integer"}}})},
        ToolDefinition{name:"task_update".into(),description:"Propose an update to canonical task state. Constraints you add are TaskConstraints (working rules you propose), not user constraints. Use supersede fields to replace outdated decisions/constraints and resolve_questions to close answered questions.".into(),input_schema:json!({"type":"object","properties":{"goal":{"type":["string","null"]},"add_constraints":{"type":"array","items":{"type":"string"}},"supersede_constraints":{"type":"array","items":{"type":"string"}},"add_decisions":{"type":"array","items":{"type":"string"}},"supersede_decisions":{"type":"array","items":{"type":"string"}},"add_hypotheses":{"type":"array","items":{"type":"string"}},"reject_hypotheses":{"type":"array","items":{"type":"string"}},"touched_files":{"type":"array","items":{"type":"string"}},"required_validations":{"type":"array","items":{"type":"string"},"description":"Requirements that must hold; their pass state is kernel evidence, not settable here"},"open_questions":{"type":["array","null"],"items":{"type":"string"}},"resolve_questions":{"type":"array","items":{"type":"string"}},"next_actions":{"type":["array","null"],"items":{"type":"string"}},"completion_criteria":{"type":"array","items":{"type":"string"}}}})},
        ToolDefinition{name:"record_evidence".into(),description:"Record an observation for a non-command claim. Only pending and unavailable statuses are accepted; passed/failed evidence is kernel-owned and comes from the validate tool.".into(),input_schema:json!({"type":"object","required":["claim","status","detail"],"properties":{"claim":{"type":"string"},"status":{"enum":["pending","unavailable"]},"detail":{"type":"string"}}})},
        ToolDefinition{name:"complete".into(),description:"State that implementation work is done. The kernel derives completion (Verified / ImplementedNotVerified / Blocked / InProgress) from this claim plus current validation evidence.".into(),input_schema:json!({"type":"object","required":["implementation_done"],"properties":{"implementation_done":{"type":"boolean"}}})}
        ,ToolDefinition{name:"spawn_agent".into(),description:"Spawn an asynchronous child Latch session with fresh context for a bounded delegated task. Returns immediately with its agent id and status.".into(),input_schema:json!({"type":"object","required":["task_name","message"],"properties":{"task_name":{"type":"string"},"message":{"type":"string"},"agent_type":{"type":"string"}}})}
        ,ToolDefinition{name:"send_agent_message".into(),description:"Queue information for an existing child without necessarily starting a new turn.".into(),input_schema:json!({"type":"object","required":["agent_id","message"],"properties":{"agent_id":{"type":"string"},"message":{"type":"string"}}})}
        ,ToolDefinition{name:"continue_agent".into(),description:"Send follow-up work to a child. Starts a new turn when idle, or delivers at the running child's next safe boundary.".into(),input_schema:json!({"type":"object","required":["agent_id","message"],"properties":{"agent_id":{"type":"string"},"message":{"type":"string"}}})}
        ,ToolDefinition{name:"wait_agents".into(),description:"Wait for final states or updates from selected children, or from any child when agent_ids is omitted. Returns their current statuses; completed reports are delivered separately as kernel notifications at the next model boundary. A timeout returns the current statuses.".into(),input_schema:json!({"type":"object","properties":{"agent_ids":{"type":"array","items":{"type":"string"}},"timeout_ms":{"type":"integer","minimum":0,"maximum":300000}}})}
        ,ToolDefinition{name:"list_agents".into(),description:"List durable child identities and current lifecycle statuses.".into(),input_schema:json!({"type":"object","properties":{}})}
        ,ToolDefinition{name:"interrupt_agent".into(),description:"Cancel a child's current turn while keeping the child reusable.".into(),input_schema:json!({"type":"object","required":["agent_id"],"properties":{"agent_id":{"type":"string"}}})}
        ,ToolDefinition{name:"close_agent".into(),description:"Cleanly shut down a child agent. Closed children cannot be reused.".into(),input_schema:json!({"type":"object","required":["agent_id"],"properties":{"agent_id":{"type":"string"}}})}
    ]);
    tools
}

/// Canonical serialization of one provider-facing request. Consecutive
/// requests in an epoch differ only by appended messages, so byte-prefix
/// comparison measures the reusable provider cache prefix.
pub(super) fn request_signature(request: &ModelRequest) -> String {
    // Tools are stable per session and precede the messages in the
    // provider-facing request, so they belong inside the reusable prefix.
    let mut signature = String::with_capacity(request.system.len() + 4096);
    signature.push_str(&request.system);
    signature.push('\u{1e}');
    signature.push_str(&serde_json::to_string(&request.tools).unwrap_or_default());
    for message in &request.messages {
        signature.push('\u{1f}');
        signature.push_str(&serde_json::to_string(message).unwrap_or_default());
    }
    signature
}

/// Byte length of the common prefix of two strings, clamped down to a valid
/// UTF-8 char boundary so callers can safely slice `&a[..shared]`.
///
/// Comparing raw bytes can land inside a multibyte character when the first
/// differing bytes are an earlier byte of two different characters (for
/// example `你` and `何` share their first two bytes). Clamping to the previous
/// boundary keeps the measurement exact: the bytes before the boundary are
/// identical and form a complete character prefix.
pub(super) fn common_prefix_bytes(a: &str, b: &str) -> usize {
    let mut shared = a
        .bytes()
        .zip(b.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while shared > 0 && !a.is_char_boundary(shared) {
        shared -= 1;
    }
    shared
}

pub(crate) fn context_messages(ctx: &crate::continuity::MaterializedContext) -> Vec<ModelMessage> {
    let raw = ctx
        .recent
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::UserMessage { text } => Some(ModelMessage::text("user", text.clone())),
            EventPayload::AgentMessageReceived { message } => {
                Some(ModelMessage::text("user", message.text.clone()))
            }
            EventPayload::AssistantMessageCompleted {
                text,
                tool_calls,
                reasoning_content,
                reasoning,
            } => Some(ModelMessage {
                role: "assistant".into(),
                content: text.clone(),
                tool_calls: tool_calls.clone(),
                tool_call_id: None,
                reasoning_content: reasoning_content.clone(),
                reasoning: reasoning.clone(),
            }),
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                Some(ModelMessage {
                    role: "tool".into(),
                    content: result.output.clone(),
                    tool_calls: vec![],
                    tool_call_id: Some(result.call_id.clone()),
                    reasoning_content: None,
                    reasoning: Vec::new(),
                })
            }
            EventPayload::RegroundRequested { signature } => Some(ModelMessage::text("user", format!("Kernel re-ground required after repeated failure {signature}. Re-read current reality, identify disproven assumptions, and form a materially different strategy before another mutation."))),
            // Durable kernel context is rendered in history order so the
            // provider-visible request stays append-only within a cache epoch.
            EventPayload::KernelContext { content, .. } => {
                Some(ModelMessage::text("user", content.clone()))
            }
            EventPayload::AgentNotificationDelivered { report } => Some(ModelMessage::text(
                "user",
                format!(
                    "Kernel child-agent report from `{}` (agent {}, status {:?}, task completion {:?}):\nsummary: {}\nfindings: {}\ntouched files: {}\nvalidation references (child-only; not root evidence): {}\nunresolved questions: {}",
                    report.task_name,
                    report.agent_id,
                    report.status,
                    report.completion,
                    report.summary,
                    report.findings.join("; "),
                    report.touched_files.join(", "),
                    report.evidence.iter().map(|item| format!("{} [{:?}] {}", item.claim, item.status, item.detail)).collect::<Vec<_>>().join("; "),
                    report.unresolved_questions.join("; "),
                ),
            )),
            // ScopeExpansionRequested is a legacy, replay-only event; it has no
            // place in the live model conversation.
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
pub(super) fn sanitize_tool_history(messages: Vec<ModelMessage>) -> Vec<ModelMessage> {
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
