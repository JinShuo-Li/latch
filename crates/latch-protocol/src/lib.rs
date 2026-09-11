#![forbid(unsafe_code)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
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
    #[serde(default)]
    pub required_validations: Vec<String>,
    #[serde(default)]
    pub validation_status: BTreeMap<String, bool>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub next_actions: Vec<String>,
    #[serde(default)]
    pub completion_criteria: Vec<String>,
    #[serde(default)]
    pub completion: CompletionState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evidence {
    pub id: Uuid,
    pub claim: String,
    pub source_event: Uuid,
    pub status: EvidenceStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Pending,
    Passed,
    Failed,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ContextStats {
    pub recent_bytes: usize,
    pub recalled_bytes: usize,
    pub canonical_bytes: usize,
    pub code_evidence_bytes: usize,
    pub reserve_bytes: usize,
    pub durable_events: usize,
    pub episodes: usize,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOwner {
    PreExisting,
    Latch,
    External,
    Extension(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelMessage {
    pub role: String,
    pub content: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: String,
    pub usage: Option<Usage>,
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
