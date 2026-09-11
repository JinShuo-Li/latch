#![forbid(unsafe_code)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

pub const EXTENSION_PROTOCOL_VERSION: &str = "0.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum Mode {
    Ask,
    Plan,
    #[default]
    Work,
}

impl Mode {
    #[must_use]
    pub const fn can_mutate(self) -> bool {
        matches!(self, Self::Work)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Ask => "ASK",
                Self::Plan => "PLAN",
                Self::Work => "WORK",
            }
        )
    }
}

impl FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "plan" => Ok(Self::Plan),
            "work" => Ok(Self::Work),
            _ => Err(format!("unknown mode {s}; expected ask, plan, or work")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    UserFact,
    UserConstraint,
    /// A working constraint proposed by the model during the task. It never
    /// masquerades as a user constraint; only actual user events can produce
    /// `UserConstraint` provenance.
    TaskConstraint,
    ObservedFact,
    Decision,
    Hypothesis,
    ModelNote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Validity {
    #[default]
    Active,
    Supported,
    Contradicted,
    Rejected,
    Stale,
    Superseded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub kind: MemoryKind,
    pub content: String,
    pub originating_event: Uuid,
    pub created_at: DateTime<Utc>,
    pub validity: Validity,
    pub confidence: Option<f32>,
    #[serde(default)]
    pub dependencies: Vec<Uuid>,
    pub supersedes: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileVersion {
    pub path: String,
    pub content_hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub output: String,
    pub is_error: bool,
    pub artifact_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Provider-reported cache-read tokens. `None` means the provider did not
    /// report the category at all, which is distinct from a reported zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Provider-reported cache-write tokens. `None` means unreported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

/// Optional user-configured per-model pricing, expressed per million tokens.
///
/// Every component is optional on purpose: a missing price component stays
/// unknown and must never be invented. The UI labels any computed amount as an
/// estimate, not a bill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelPricing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_million: Option<f64>,
    #[serde(default = "default_currency")]
    pub currency: String,
}

fn default_currency() -> String {
    "USD".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventPayload {
    UserMessage {
        text: String,
    },
    AssistantMessageCompleted {
        text: String,
        tool_calls: Vec<ToolCall>,
        /// Opaque reasoning emitted by reasoning-capable OpenAI-compatible
        /// models (for example DeepSeek). It is persisted so that later
        /// requests can replay it verbatim for tool-call turns. `None` means
        /// no reasoning was present or the field is not applicable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    ModelRequestStarted {
        provider: String,
        model: String,
    },
    ModelRequestFinished {
        stop_reason: String,
    },
    ToolRequested {
        call: ToolCall,
    },
    PermissionDecision {
        tool: String,
        decision: String,
        reason: String,
    },
    ToolStarted {
        call_id: String,
        tool: String,
    },
    ToolCompleted {
        result: ToolResult,
    },
    ToolFailed {
        result: ToolResult,
    },
    FileObserved {
        version: FileVersion,
    },
    FileChanged {
        before: Option<FileVersion>,
        after: FileVersion,
        owner: ChangeOwner,
        /// Content-addressed artifact (relative to the session artifact store)
        /// holding the pre-change bytes so ownership and undo survive resume.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        undo_artifact: Option<String>,
        /// Line delta recorded by the kernel change ledger. Older events
        /// default to zero because the counts were not stored.
        #[serde(default)]
        additions: usize,
        #[serde(default)]
        deletions: usize,
    },
    /// Records workspace drift observed around a shell execution. Emitted when
    /// a shell (or validation) command mutated paths outside the guarded edit
    /// tools. `reversible` states honestly whether Latch captured enough
    /// pre-change state to restore them.
    ShellMutationObserved {
        command: String,
        reversible: bool,
        paths: Vec<String>,
    },
    /// Tombstone marking that a previously recorded owned change was reverted.
    /// Ledger reconstruction after resume uses it to drop the undone entry.
    ChangeReverted {
        path: String,
        content_hash: String,
    },
    ExternalFileChangeDetected {
        path: String,
        expected_hash: String,
        actual_hash: String,
    },
    GitStateObserved {
        head: Option<String>,
        dirty_paths: Vec<String>,
    },
    CheckpointCreated {
        id: Uuid,
        label: String,
    },
    TaskStateUpdated {
        state: TaskState,
    },
    /// The effective session mode changed (for example via `/mode`). Durable so
    /// `--resume` restores the mode the session actually ended in.
    ModeChanged {
        mode: Mode,
    },
    /// The kernel recomputed completion and the derived value changed. This is
    /// the only place completion truth is announced; the model never sets it.
    CompletionChanged {
        completion: CompletionState,
    },
    EvidenceCreated {
        evidence: Evidence,
    },
    ValidationResult {
        command: String,
        passed: bool,
        detail: String,
    },
    FailureAttempt {
        signature: String,
        count: u32,
    },
    RegroundRequested {
        signature: String,
    },
    /// Kernel-owned progress stagnation: consecutive model turns repeated
    /// observations whose results have not changed in the current progress
    /// epoch. `unchanged` lists the semantic labels of those observations so
    /// the next request can explicitly tell the model not to inspect them
    /// again. Further repeats after this event are suppressed with a synthetic
    /// terminal tool result rather than executed.
    ProgressStagnation {
        unchanged: Vec<String>,
        redundant_turns: u32,
    },
    ScopeExpansionRequested {
        mutations: usize,
        reason: String,
    },
    ContextMaterialized {
        stats: ContextStats,
    },
    ContextMemoryRecalled {
        query: String,
        memory_ids: Vec<Uuid>,
        event_ids: Vec<Uuid>,
    },
    ManualCompact {
        generation: u32,
    },
    SessionResumed,
    ModelUsage {
        usage: Usage,
    },
    OperationInterrupted {
        operation_id: Uuid,
        description: String,
    },
    ExtensionEvent {
        extension: String,
        method: String,
        payload: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: Uuid,
    pub session_id: Uuid,
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub parent_id: Option<Uuid>,
    pub payload: EventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompletionState {
    #[default]
    InProgress,
    ImplementedNotVerified,
    Verified,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Hypothesis {
    pub text: String,
    pub validity: Validity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TaskState {
    pub goal: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub hypotheses: Vec<Hypothesis>,
    #[serde(default)]
    pub rejected_hypotheses: Vec<String>,
    #[serde(default)]
    pub touched_files: Vec<String>,
    /// Requirements the model declared (or the kernel registered when `validate`
    /// ran). Whether each requirement currently passes is kernel-owned evidence,
    /// never a model-writable flag.
    #[serde(default)]
    pub required_validations: Vec<String>,
    /// The model's implementation claim. Completion itself is derived by the
    /// kernel from this claim plus current evidence state.
    #[serde(default)]
    pub implementation_done: bool,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub next_actions: Vec<String>,
    #[serde(default)]
    pub completion_criteria: Vec<String>,
    #[serde(default)]
    pub completion: CompletionState,
}

/// One evidence observation. Entries are append-only and immutable; the
/// *current* evidence for a claim is the newest entry for that claim, so a
/// historical failure never poisons a requirement that now passes. The raw
/// event log retains every attempt either way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evidence {
    pub id: Uuid,
    /// Semantic requirement or claim the evidence is about, for example
    /// `"existing unittest passes"`. Claims are matched case-insensitively.
    pub claim: String,
    /// Kernel-internal provenance: the durable event that produced this
    /// evidence. Never exposed to the model as an input.
    pub source_event: Uuid,
    pub status: EvidenceStatus,
    pub detail: String,
    pub created_at: DateTime<Utc>,
    /// The evidence entry this entry supersedes, when it updates an existing
    /// claim's current state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Pending,
    Passed,
    Failed,
    Unavailable,
}

/// Token-native accounting of the complete request Latch is about to send.
///
/// Every number is a pre-request *estimate* produced by the kernel's
/// conservative token estimator. Once a request completes, the provider's
/// reported usage (see [`Usage`]) is authoritative. Old durable events predate
/// these fields and deserialize as zeros.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ContextStats {
    /// Compiled system/kernel instructions.
    pub instructions_tokens: usize,
    /// Canonical task state, evidence, failure lineages, and durable memory.
    pub state_tokens: usize,
    /// Verbatim recent transcript, protocol-atomic.
    pub recent_tokens: usize,
    /// Recalled original events plus the scored episode index.
    pub recall_tokens: usize,
    /// Tool schemas actually sent with this request.
    pub tools_tokens: usize,
    /// Extension-provided context sources.
    pub extension_tokens: usize,
    /// Estimated total for the complete request (sum of the above).
    pub total_tokens: usize,
    /// Request budget: context window minus the output/safety reserve.
    pub budget_tokens: usize,
    /// The model's full context window, when known.
    pub window_tokens: usize,
    /// Tokens reserved for the model response and safety.
    pub reserve_tokens: usize,
    /// Remaining request budget after this materialization (`budget - total`).
    pub headroom_tokens: usize,
    pub durable_events: usize,
    pub episodes: usize,
    pub selected_episodes: usize,
    /// True while these numbers are estimates awaiting provider-reported usage.
    pub estimated: bool,
    pub status: String,
}

impl ContextStats {
    /// Recomputes the derived totals after tools/extension costs are added.
    pub fn recompute(&mut self) {
        self.total_tokens = self
            .instructions_tokens
            .saturating_add(self.state_tokens)
            .saturating_add(self.recent_tokens)
            .saturating_add(self.recall_tokens)
            .saturating_add(self.tools_tokens)
            .saturating_add(self.extension_tokens);
        self.headroom_tokens = self.budget_tokens.saturating_sub(self.total_tokens);
        self.status = if self.budget_tokens == 0 || self.total_tokens <= self.budget_tokens {
            "bounded".to_owned()
        } else {
            "over_budget".to_owned()
        };
        self.estimated = true;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOwner {
    PreExisting,
    Latch,
    /// Mutation produced by a shell or validation command executed inside the
    /// workspace (for example `cargo fmt` or a generator). Classified honestly
    /// as tool-originated rather than pretending it pre-existed or was
    /// externally authored.
    Shell,
    External,
    Extension(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelMessage {
    pub role: String,
    pub content: String,
    /// Tool calls proposed by an assistant message. Empty for every other role.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Correlation id for a `role: "tool"` result message. Providers map this
    /// onto their native tool-result linkage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Reasoning that accompanied an assistant turn. Providers that support
    /// reasoning replay it; others ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl ModelMessage {
    #[must_use]
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelRequest {
    pub system: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: String,
    pub usage: Option<Usage>,
    /// Reasoning returned by a reasoning-capable model, preserved verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum StreamEvent {
    TextDelta(String),
    ToolCallDelta(ToolCall),
    Usage(Usage),
    Completed(ModelResponse),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcMessage {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

/// Run-phase of one tool invocation as shown to the user. A single visual item
/// progresses from `Running` to `Passed` or `Failed`; the transcript upserts by
/// `call_id` instead of appending a second row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRunStatus {
    Running,
    Passed,
    Failed,
}

/// One user-visible transcript element. Durable events map onto these through
/// [`display_items`], shared by live rendering and resume replay, so both paths
/// format history identically. Hidden internals (reasoning content, context
/// statistics, model usage, raw task state) never become display items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DisplayItem {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolActivity {
        call_id: String,
        verb: String,
        target: String,
        detail: String,
        status: ToolRunStatus,
    },
    KernelNotice {
        text: String,
    },
    Error {
        text: String,
    },
}

impl DisplayItem {
    /// `call_id` when this item is a tool activity row, used to update an
    /// existing lifecycle row in place.
    #[must_use]
    pub fn call_id(&self) -> Option<&str> {
        match self {
            Self::ToolActivity { call_id, .. } => Some(call_id),
            _ => None,
        }
    }
}

fn compact_detail(text: &str, limit: usize) -> String {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    first.chars().take(limit).collect()
}

/// Converts one durable event into the user-visible transcript items it
/// implies. This is the single presentation formatter: live rendering and
/// `--resume` replay both call it, so history and live views always agree.
/// Internal bookkeeping events map to nothing.
#[must_use]
pub fn display_items(event: &Event) -> Vec<DisplayItem> {
    match &event.payload {
        EventPayload::UserMessage { text } => vec![DisplayItem::UserMessage { text: text.clone() }],
        EventPayload::AssistantMessageCompleted { text, .. } => {
            vec![DisplayItem::AssistantMessage { text: text.clone() }]
        }
        EventPayload::ToolRequested { call } => vec![DisplayItem::ToolActivity {
            call_id: call.id.clone(),
            verb: call.name.clone(),
            target: compact_tool_target(&call.arguments),
            detail: String::new(),
            status: ToolRunStatus::Running,
        }],
        EventPayload::ToolCompleted { result } => vec![DisplayItem::ToolActivity {
            call_id: result.call_id.clone(),
            verb: result.name.clone(),
            target: String::new(),
            detail: compact_detail(&result.output, 80),
            status: ToolRunStatus::Passed,
        }],
        EventPayload::ToolFailed { result } => vec![DisplayItem::ToolActivity {
            call_id: result.call_id.clone(),
            verb: result.name.clone(),
            target: String::new(),
            detail: compact_detail(&result.output, 80),
            status: ToolRunStatus::Failed,
        }],
        EventPayload::PermissionDecision { decision, .. } if decision != "Allow" => {
            // The denial itself surfaces through the failed tool result.
            vec![]
        }
        EventPayload::ModeChanged { mode } => vec![DisplayItem::KernelNotice {
            text: format!("mode: {mode}"),
        }],
        EventPayload::CompletionChanged { completion } => vec![DisplayItem::KernelNotice {
            text: format!("completion: {completion:?}"),
        }],
        EventPayload::ValidationResult {
            command,
            passed,
            detail,
        } => {
            // Validation results surface through the validate tool row; keep
            // the raw event durable but do not double-render it.
            let _ = (command, passed, detail);
            vec![]
        }
        EventPayload::RegroundRequested { signature } => vec![DisplayItem::KernelNotice {
            text: format!("re-ground requested after repeated failure of {signature}"),
        }],
        EventPayload::ProgressStagnation {
            unchanged,
            redundant_turns,
        } => vec![DisplayItem::KernelNotice {
            text: format!(
                "inspection stagnation: {} observation(s) unchanged across {redundant_turns} redundant turn(s); re-ground requested",
                unchanged.len()
            ),
        }],
        EventPayload::ScopeExpansionRequested { reason, .. } => vec![DisplayItem::KernelNotice {
            text: format!("scope review: {reason}"),
        }],
        EventPayload::SessionResumed => vec![DisplayItem::KernelNotice {
            text: "session resumed".into(),
        }],
        EventPayload::OperationInterrupted { description, .. } => {
            vec![DisplayItem::KernelNotice {
                text: format!("interrupted operation reported: {description}"),
            }]
        }
        EventPayload::ManualCompact { .. } => vec![DisplayItem::KernelNotice {
            text: "active context reset; durable history and state retained".into(),
        }],
        EventPayload::ShellMutationObserved {
            command,
            reversible,
            paths,
        } => {
            if paths.is_empty() && !reversible {
                vec![DisplayItem::KernelNotice {
                    text: format!(
                        "shell mutation detection unavailable for `{command}`; changes are not undoable"
                    ),
                }]
            } else {
                vec![DisplayItem::KernelNotice {
                    text: format!(
                        "shell mutated {} path(s) that Latch cannot undo: {}",
                        paths.len(),
                        paths.join(", ")
                    ),
                }]
            }
        }
        EventPayload::ChangeReverted { path, .. } => vec![DisplayItem::KernelNotice {
            text: format!("reverted {path}"),
        }],
        // Evidence, task state, file versions, context statistics, provider
        // bookkeeping, and extension traffic are durable truth but not
        // user-facing transcript rows.
        _ => vec![],
    }
}

/// Compact human target for a tool row: the path, command, or query argument.
#[must_use]
pub fn compact_tool_target(arguments: &Value) -> String {
    arguments
        .get("path")
        .or_else(|| arguments.get("query"))
        .or_else(|| arguments.get("requirement"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            arguments
                .get("command")
                .and_then(Value::as_str)
                .map(|command| compact_detail(command, 56))
        })
        .unwrap_or_default()
}
