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

/// Stable identity of a configured provider instance. A provider is a
/// user-named entry in the provider registry (for example `opencode-go`,
/// `deepseek`, or a custom name); it is not derived from a base URL.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Provider-neutral reasoning effort. `ProviderDefault` means Latch does not
/// select a value and lets the model use its own default; the other variants
/// are only ever emitted when the resolved model capability lists them.
///
/// The set mirrors the union of current provider controls (OpenAI
/// `none|minimal|low|medium|high|xhigh|max`, Anthropic
/// `low|medium|high|xhigh|max`, DeepSeek `low|high|max`). No provider supports
/// every value; the model capability descriptor decides what is selectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    #[default]
    ProviderDefault,
    None,
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl ReasoningEffort {
    /// Every concrete level in increasing depth order. `ProviderDefault` is a
    /// selection state, not a level.
    pub const LEVELS: [Self; 7] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// The exact value sent on the wire when this effort is selected. `None`
    /// means the provider payload must not carry an effort field at all.
    #[must_use]
    pub const fn wire(self) -> Option<&'static str> {
        match self {
            Self::ProviderDefault => None,
            Self::None => Some("none"),
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
            Self::Max => Some("max"),
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ProviderDefault => "provider default",
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    #[must_use]
    pub const fn short(self) -> &'static str {
        match self {
            Self::ProviderDefault => "default",
            other => other.label(),
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for ReasoningEffort {
    type Err = String;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "default" | "provider-default" | "provider_default" | "auto" => {
                Ok(Self::ProviderDefault)
            }
            "none" | "off" | "disabled" => Ok(Self::None),
            "minimal" | "min" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" | "med" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" | "x-high" | "extra-high" | "extended" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            other => Err(format!("unknown reasoning effort {other:?}")),
        }
    }
}

/// The effective inference selection of a session: which configured provider,
/// which model on that provider, and which reasoning effort. Credentials are
/// deliberately not part of a profile; they are resolved separately so a
/// resumed session never persists secret material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InferenceProfile {
    pub provider: ProviderId,
    pub model: String,
    pub effort: ReasoningEffort,
}

impl InferenceProfile {
    #[must_use]
    pub fn new(
        provider: impl Into<ProviderId>,
        model: impl Into<String>,
        effort: ReasoningEffort,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            effort,
        }
    }
}

impl fmt::Display for InferenceProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} ({})",
            self.provider,
            self.model,
            self.effort.label()
        )
    }
}

/// How cautious the kernel is when classifying a proposed capability. Mode is
/// what kind of work is allowed; Safety decides Allow, Ask, or Deny. They are
/// orthogonal: changing Safety never changes what Mode permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Safety {
    Strict,
    #[default]
    Standard,
    Autonomous,
}

impl Safety {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Standard => "Standard",
            Self::Autonomous => "Autonomous",
        }
    }

    #[must_use]
    pub const fn short(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Standard => "std",
            Self::Autonomous => "auto",
        }
    }
}

impl fmt::Display for Safety {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for Safety {
    type Err = String;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "strict" => Ok(Self::Strict),
            "standard" | "std" => Ok(Self::Standard),
            "autonomous" | "auto" => Ok(Self::Autonomous),
            other => Err(format!("unknown safety profile {other:?}")),
        }
    }
}

/// How an `Ask` is resolved. Orthogonal to both Mode and Safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Automatically resolve Ask as approved, always recording the normal
    /// request/resolution provenance. Never overrides Deny.
    AutoApprove,
    /// A real human decision through the approval UI.
    #[default]
    Human,
    /// A separate stateless model review of the proposed command.
    AiReview,
}

impl PermissionMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AutoApprove => "All approved",
            Self::Human => "Approved by ask",
            Self::AiReview => "Approve for me",
        }
    }

    #[must_use]
    pub const fn short(self) -> &'static str {
        match self {
            Self::AutoApprove => "auto",
            Self::Human => "ask",
            Self::AiReview => "ai",
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for PermissionMode {
    type Err = String;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "all" | "all_approved" | "auto_approve" => Ok(Self::AutoApprove),
            "ask" | "human" | "approved_by_ask" => Ok(Self::Human),
            "ai" | "review" | "ai_review" | "approve_for_me" => Ok(Self::AiReview),
            other => Err(format!("unknown permission mode {other:?}")),
        }
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

/// Durable lifecycle state for a child Latch session. `Interrupted` keeps the
/// child reusable; only `Closed` permanently shuts its worker down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Starting,
    Running,
    Completed,
    Interrupted,
    Failed,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageKind {
    Information,
    FollowUp,
}

/// Stable identity and topology metadata for one independently persisted
/// child session. The agent id is its session id by design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub agent_id: Uuid,
    pub root_session_id: Uuid,
    pub parent_session_id: Uuid,
    pub task_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    pub depth: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub message_id: Uuid,
    pub kind: AgentMessageKind,
    pub text: String,
}

/// A semantic evidence reference in a child report. It deliberately omits the
/// kernel's internal evidence/event ids and never becomes evidence in another
/// session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEvidenceRef {
    pub claim: String,
    pub status: EvidenceStatus,
    pub detail: String,
}

/// Compact terminal output from one child turn. The full child transcript
/// remains available in its own session and is never copied into its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReport {
    pub report_id: Uuid,
    pub agent_id: Uuid,
    pub task_name: String,
    pub status: AgentStatus,
    pub completion: CompletionState,
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<String>,
    #[serde(default)]
    pub touched_files: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<AgentEvidenceRef>,
    #[serde(default)]
    pub unresolved_questions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    /// Total prompt tokens as reported by the provider. For OpenAI/DeepSeek
    /// style usage this includes cache-read tokens; for Anthropic it is the
    /// uncached portion, with cache reads and writes reported separately.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Provider-reported cache-read (hit) tokens. `None` means the provider did
    /// not report the category at all, which is distinct from a reported zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Provider-reported cache-write tokens. `None` means unreported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Provider-reported cache-miss (uncached) input tokens, normalized by the
    /// adapter. `None` means the provider did not report it; it can then be
    /// derived when the cache-read category is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_miss_tokens: Option<u64>,
    /// Provider-reported reasoning/thinking tokens generated for this request.
    /// `None` means the provider did not report the category; it is never
    /// derived from output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// Uncached input tokens when known: the explicit miss count, or the total
    /// input minus the known cache-read count. `None` stays unknown rather than
    /// guessing.
    #[must_use]
    pub fn uncached_input_tokens(&self) -> Option<u64> {
        self.cache_miss_tokens.or_else(|| {
            self.cache_read_tokens
                .map(|read| self.input_tokens.saturating_sub(read))
        })
    }
}

/// One provider-visible reasoning artifact attached to an assistant turn.
///
/// Reasoning is provider-specific opaque material that must be replayed
/// byte-for-byte when the provider requires it (DeepSeek `reasoning_content`,
/// OpenAI encrypted reasoning items, Anthropic thinking/redacted-thinking
/// blocks). It is durable and replayable but never rendered as assistant text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReasoningArtifact {
    /// Plain reasoning text with no replay requirement beyond the text itself
    /// (for example a DeepSeek `reasoning_content` value).
    Text {
        #[serde(default)]
        text: String,
    },
    /// Provider-encrypted opaque reasoning that must be echoed back verbatim
    /// (OpenAI Responses `reasoning.encrypted_content`).
    Encrypted {
        #[serde(default)]
        data: String,
    },
    /// Anthropic thinking block: readable summary text plus the opaque
    /// `signature` that authenticates the original block.
    Thinking {
        #[serde(default)]
        text: String,
        #[serde(default)]
        signature: String,
    },
    /// Anthropic redacted thinking block: fully opaque `data`.
    Redacted {
        #[serde(default)]
        data: String,
    },
}

impl ReasoningArtifact {
    /// Tokens this artifact contributes to a replayed request, priced by the
    /// caller's estimator over its provider-visible payload.
    #[must_use]
    pub fn replay_text(&self) -> &str {
        match self {
            Self::Text { text } | Self::Thinking { text, .. } => text,
            Self::Encrypted { data } | Self::Redacted { data } => data,
        }
    }

    /// Whether this artifact must be replayed to the provider unchanged.
    #[must_use]
    pub const fn requires_replay(&self) -> bool {
        true
    }
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

/// What kind of kernel-owned authoritative context one message carries. The
/// event is durable so a request within a cache epoch is an exact prefix of the
/// next; the newest snapshot or state update is the current truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelContextKind {
    /// Complete authoritative state at a cache-epoch start.
    Snapshot,
    /// Complete authoritative state after a material change during an epoch.
    StateUpdate,
    /// Recalled original historical events for the current instruction.
    Recall,
    /// Navigational archival episode index.
    EpisodeIndex,
    /// Kernel re-ground instruction after repeated failure/stagnation.
    Reground,
    /// Extension-provided context sources.
    Extension,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventPayload {
    /// First event in a child session. Session creation and this durable edge
    /// commit in one store transaction.
    AgentSpawned {
        identity: AgentIdentity,
        delegation_brief: String,
    },
    AgentMessageQueued {
        message: AgentMessage,
    },
    /// A queued parent message accepted by the child loop at a safe model
    /// boundary. This single event is both the resume receipt and the
    /// provider-visible user turn.
    AgentMessageReceived {
        message: AgentMessage,
    },
    AgentStatusChanged {
        status: AgentStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    AgentReportCreated {
        report: AgentReport,
    },
    AgentInterruptRequested,
    AgentInterrupted {
        reason: String,
    },
    AgentCloseRequested,
    AgentClosed,
    /// The only agent-graph event rendered into a parent's model context. It
    /// is appended by the parent loop at a safe model boundary.
    AgentNotificationDelivered {
        report: AgentReport,
    },
    UserMessage {
        text: String,
    },
    /// One root user-request execution began. Run boundaries are explicit and
    /// durable so per-run accounting never has to infer duration or totals
    /// from session-wide timestamps.
    RunStarted {
        run_id: Uuid,
        /// First user text of the run, for cheap display. The authoritative
        /// prompt remains the `UserMessage` event.
        #[serde(default)]
        prompt: String,
    },
    /// The run terminated. `outcome` is `completed`, `cancelled`, or `error`.
    RunCompleted {
        run_id: Uuid,
        outcome: String,
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
        /// Provider-specific reasoning artifacts (encrypted reasoning,
        /// Anthropic thinking/redacted blocks). Empty for providers that only
        /// use `reasoning_content`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasoning: Vec<ReasoningArtifact>,
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
    /// A policy or extension guard asked for human approval. The request is
    /// durable and is resolved exactly once by a real user decision (or marked
    /// expired on resume); the model can never fabricate approval.
    PermissionRequested {
        request_id: Uuid,
        tool: String,
        #[serde(default)]
        arguments: Value,
        reason: String,
        /// Capability names the operation needs, for the approval UI and the
        /// durable audit trail.
        #[serde(default)]
        capabilities: Vec<String>,
    },
    PermissionResolved {
        request_id: Uuid,
        approved: bool,
        /// `user`, `cancelled`, `non_interactive`, `resume_expired`, `auto`,
        /// or `ai`.
        source: String,
        /// AI reviewer risk level (`low`/`medium`/`high`/`critical`) when the
        /// resolution came from the stateless reviewer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        risk: Option<String>,
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
        /// True when the path did not exist before this change, so undo means
        /// deletion. Legacy events default to false; undo refuses rather than
        /// guessing and deleting a pre-existing file.
        #[serde(default)]
        created: bool,
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
        /// Bounded unified-diff preview computed from the real before/after
        /// bytes at mutation time. Empty for legacy events and shell drift.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        preview: String,
        /// The guarded tool call that produced this change, so the transcript
        /// can attribute multi-edit batches precisely.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
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
    /// The effective safety profile changed via `/safety`. Durable so resume
    /// restores exactly the profile the session ended in.
    SafetyChanged {
        safety: Safety,
    },
    /// The effective permission resolver changed via `/permissions`.
    PermissionsChanged {
        mode: PermissionMode,
    },
    /// The effective inference profile (provider, model, reasoning effort)
    /// changed. Durable so `--resume` restores the profile the session ended
    /// with. Credentials are never part of this event.
    InferenceProfileChanged {
        provider: ProviderId,
        model: String,
        #[serde(default)]
        effort: ReasoningEffort,
        #[serde(default)]
        reason: String,
    },
    /// The recent working set reached its budget and Latch advanced to a new
    /// append-only context epoch. Non-destructive: every earlier raw event
    /// remains durable and recallable. The new epoch starts at the event with
    /// `from_sequence` (itself still present in the raw log).
    ContextEpochStarted {
        from_sequence: u64,
        reason: String,
        /// Cache-epoch generation; increments on every rotation.
        #[serde(default)]
        generation: u64,
        /// Working-memory tokens retained after the rotation.
        #[serde(default)]
        retained_tokens: usize,
    },
    /// One kernel-owned authoritative context message as it was sent to the
    /// provider. Persisting it keeps every request within a cache epoch an
    /// append-only extension of the previous request, and lets resume replay
    /// the exact provider-visible kernel history. The newest snapshot or state
    /// update carries the complete current state; earlier ones are provenance.
    KernelContext {
        generation: u64,
        revision: u64,
        kind: KernelContextKind,
        content: String,
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
    /// Legacy replay-only event from the removed scope-expansion restriction.
    /// Never emitted; retained so historical sessions still deserialize.
    ScopeExpansionRequested {
        mutations: usize,
        reason: String,
    },
    ContextMaterialized {
        stats: ContextStats,
    },
    /// A long-running development process was started and is owned by the
    /// kernel. `ProcessExited` closes the lifecycle; a start without an exit
    /// means the session restarted while the process was still running.
    ProcessStarted {
        id: String,
        command: String,
        #[serde(default)]
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    ProcessExited {
        id: String,
        /// `exit <code>`, `killed`, or `lost`.
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact_id: Option<String>,
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

impl CompletionState {
    /// Completion the model has landed on: the implementation claim is made and
    /// no further kernel work is pending. `InProgress` is the only non-terminal
    /// state. `Blocked` remains terminal for the current turn (the kernel has
    /// recorded a required validation that could not run) but still requires
    /// the model to report the blockage; callers decide whether to draw another
    /// turn.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        !matches!(self, Self::InProgress)
    }
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
    /// Visible user/assistant conversation text inside the recent window. A
    /// partition of `recent_tokens`, never added to the total separately.
    #[serde(default)]
    pub conversation_tokens: usize,
    /// Replayed assistant reasoning inside the recent window. A partition of
    /// `recent_tokens`; it is what reasoning-replay costs on each request.
    #[serde(default)]
    pub reasoning_replay_tokens: usize,
    /// Assistant tool-call names and arguments inside the recent window. A
    /// partition of `recent_tokens`.
    #[serde(default)]
    pub tool_arguments_tokens: usize,
    /// Tool results inside the recent window. A partition of `recent_tokens`.
    #[serde(default)]
    pub tool_result_tokens: usize,
    /// Recalled original events plus the scored episode index.
    pub recall_tokens: usize,
    /// Estimated tokens of the archival episode index alone. This is a subset
    /// of `recall_tokens` and is never added to the total separately; it lets
    /// an observer distinguish archival metadata from recalled originals.
    #[serde(default)]
    pub episode_tokens: usize,
    /// First durable sequence retained in recent working memory; 0 when
    /// working memory is empty. An advancing value explains when and where the
    /// recent window moved under budget pressure.
    #[serde(default)]
    pub recent_start_sequence: u64,
    /// Tokens that left recent working memory since the previous
    /// materialization, explaining a drop in `recent_tokens`.
    #[serde(default)]
    pub recent_evicted_tokens: usize,
    /// Cache-epoch generation currently in use (0 before the first rotation).
    #[serde(default)]
    pub cache_epoch: u64,
    /// Estimated provider-visible tokens in the current cache epoch.
    #[serde(default)]
    pub cache_epoch_tokens: usize,
    /// User turns in the current cache epoch (its conversation span).
    #[serde(default)]
    pub cache_epoch_turns: u64,
    /// Why the current epoch was established.
    #[serde(default)]
    pub cache_rotation_reason: String,
    /// Working-memory tokens retained when the current epoch was established.
    #[serde(default)]
    pub cache_rotation_retained_tokens: usize,
    /// Tool schemas actually sent with this request.
    pub tools_tokens: usize,
    /// Extension-provided context sources.
    pub extension_tokens: usize,
    /// Estimated total for the complete request (sum of the above).
    pub total_tokens: usize,
    /// Estimated size of the exact provider-facing request that was assembled:
    /// system (including canonical/recalled/extension blocks), messages with
    /// tool calls, arguments, and replayed reasoning, plus tool schemas.
    #[serde(default)]
    pub request_tokens: usize,
    /// Estimated architecture cacheability: exact byte prefix shared with the
    /// previous request, measured on Latch's canonical serialization
    /// (system + messages + tools) and priced with the kernel's conservative
    /// token estimator. This is a diagnostic for Latch's own request layout,
    /// not the provider's tokenizer or wire representation, so it does not
    /// predict an exact provider cache hit. Provider-reported cache-read/hit
    /// usage remains authoritative.
    #[serde(default)]
    pub common_prefix_tokens: usize,
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
    /// Provider-specific reasoning artifacts that must be replayed unchanged
    /// (Anthropic thinking/redacted blocks, OpenAI encrypted reasoning).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<ReasoningArtifact>,
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
            reasoning: Vec::new(),
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
    /// Provider-specific reasoning artifacts (Anthropic thinking, OpenAI
    /// encrypted reasoning) preserved for exact replay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<ReasoningArtifact>,
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
        // Safety and permissions are chrome state, shown in the composer; they
        // do not belong in the durable transcript.
        EventPayload::SafetyChanged { .. } | EventPayload::PermissionsChanged { .. } => Vec::new(),
        EventPayload::InferenceProfileChanged {
            provider,
            model,
            effort,
            ..
        } => vec![DisplayItem::KernelNotice {
            text: format!("inference profile: {provider}/{model} ({})", effort.label()),
        }],
        EventPayload::ContextEpochStarted { .. } => vec![DisplayItem::KernelNotice {
            text: "cache epoch rotated; earlier events remain durable and searchable".into(),
        }],
        // Kernel context is authoritative provider-facing state, not transcript
        // chrome; it stays durable and replayable without double-rendering.
        EventPayload::KernelContext { .. } => Vec::new(),
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
        EventPayload::ProcessStarted {
            id, command, label, ..
        } => vec![DisplayItem::KernelNotice {
            text: if label.is_empty() {
                format!("process {id} started: {command}")
            } else {
                format!("process {id} started ({label}): {command}")
            },
        }],
        EventPayload::ProcessExited { id, status, .. } => vec![DisplayItem::KernelNotice {
            text: format!("process {id} {status}"),
        }],
        EventPayload::PermissionRequested { tool, reason, .. } => {
            vec![DisplayItem::KernelNotice {
                text: format!("permission requested: {tool} — {reason}"),
            }]
        }
        EventPayload::PermissionResolved {
            approved, source, ..
        } => vec![DisplayItem::KernelNotice {
            text: if *approved {
                format!("permission approved ({source})")
            } else {
                format!("permission denied ({source})")
            },
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
        // Legacy replay-only scope event; the restriction was removed, so it
        // contributes nothing to displayed history.
        EventPayload::ScopeExpansionRequested { .. } => Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_parses_and_round_trips() {
        for (raw, effort) in [
            ("none", ReasoningEffort::None),
            ("minimal", ReasoningEffort::Minimal),
            ("min", ReasoningEffort::Minimal),
            ("low", ReasoningEffort::Low),
            ("medium", ReasoningEffort::Medium),
            ("high", ReasoningEffort::High),
            ("xhigh", ReasoningEffort::XHigh),
            ("extra-high", ReasoningEffort::XHigh),
            ("max", ReasoningEffort::Max),
            ("provider-default", ReasoningEffort::ProviderDefault),
            ("provider_default", ReasoningEffort::ProviderDefault),
            ("default", ReasoningEffort::ProviderDefault),
        ] {
            assert_eq!(raw.parse::<ReasoningEffort>().unwrap(), effort, "{raw}");
        }
        assert!("ultra".parse::<ReasoningEffort>().is_err());
        assert_eq!(ReasoningEffort::Low.wire(), Some("low"));
        assert_eq!(ReasoningEffort::XHigh.wire(), Some("xhigh"));
        assert_eq!(ReasoningEffort::ProviderDefault.wire(), None);
        assert_eq!(ReasoningEffort::LEVELS.len(), 7);
        let json = serde_json::to_string(&ReasoningEffort::Max).unwrap();
        assert_eq!(json, "\"max\"");
        assert_eq!(
            serde_json::to_string(&ReasoningEffort::XHigh).unwrap(),
            "\"xhigh\"",
            "the wire/config spelling stays xhigh"
        );
        // Legacy configurations only ever used these four values.
        for legacy in ["low", "high", "max", "default"] {
            assert!(legacy.parse::<ReasoningEffort>().is_ok());
        }
    }

    #[test]
    fn reasoning_artifacts_round_trip_and_never_render_as_text() {
        let artifacts = vec![
            ReasoningArtifact::Text {
                text: "deepseek reasoning".into(),
            },
            ReasoningArtifact::Encrypted {
                data: "opaque-encrypted".into(),
            },
            ReasoningArtifact::Thinking {
                text: "summary".into(),
                signature: "sig".into(),
            },
            ReasoningArtifact::Redacted {
                data: "redacted".into(),
            },
        ];
        let json = serde_json::to_string(&artifacts).unwrap();
        assert!(json.contains("\"kind\":\"thinking\""));
        assert!(json.contains("\"kind\":\"redacted\""));
        let back: Vec<ReasoningArtifact> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, artifacts);
        assert_eq!(artifacts[0].replay_text(), "deepseek reasoning");
        assert_eq!(artifacts[3].replay_text(), "redacted");
    }

    #[test]
    fn inference_profile_is_provider_neutral_and_credential_free() {
        let profile =
            InferenceProfile::new("opencode-go", "deepseek-v4.1-flash", ReasoningEffort::Low);
        let json = serde_json::to_string(&profile).unwrap();
        assert!(json.contains("opencode-go"));
        assert!(!json.contains("key"));
        assert!(!json.contains("api"));
        let back: InferenceProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, profile);
    }

    #[test]
    fn completion_states_distinguish_terminal_from_in_progress() {
        assert!(!CompletionState::InProgress.is_terminal());
        assert!(CompletionState::ImplementedNotVerified.is_terminal());
        assert!(CompletionState::Verified.is_terminal());
        assert!(CompletionState::Blocked.is_terminal());
    }
}
