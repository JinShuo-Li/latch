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

/// Provider-neutral input modality. Every model accepts text; image input is an
/// explicit capability and is never inferred from a model name. The enum is
/// small on purpose so another modality can be added without another
/// architectural change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    Text,
    Image,
}

impl InputModality {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
        }
    }
}

impl fmt::Display for InputModality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Kind of durable media attached to a message or tool result. Only images
/// exist in this version; the field keeps the door open for later modalities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
}

impl MediaKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
        }
    }
}

/// Compact, durable reference to one immutable media artifact in Latch's
/// session artifact store.
///
/// The bytes live only in artifact storage; events, messages, logs, and
/// transcripts carry this metadata alone. `id` and `sha256` are the content
/// hash, so re-ingesting identical bytes deduplicates to one artifact.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRef {
    /// Content-addressed identity (`sha256` hex).
    pub id: String,
    pub kind: MediaKind,
    /// Detected media type, for example `image/png`. Never trusted from the
    /// file name alone.
    pub mime_type: String,
    /// Immutable artifact path, relative to the session artifact store.
    pub artifact_path: String,
    pub sha256: String,
    pub byte_len: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Original user-facing name (for example the workspace-relative path or
    /// the attached file name). Presentation only; never a security boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl std::fmt::Debug for MediaRef {
    /// Media metadata is safe to log; the bytes themselves never live in a
    /// `MediaRef`, so nothing here can leak image content.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaRef")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("mime_type", &self.mime_type)
            .field("artifact_path", &self.artifact_path)
            .field("sha256", &self.sha256)
            .field("byte_len", &self.byte_len)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("display_name", &self.display_name)
            .finish()
    }
}

impl MediaRef {
    /// Human-facing name, falling back to the content id.
    #[must_use]
    pub fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.id)
    }

    /// `WIDTHxHEIGHT` when dimensions are known.
    #[must_use]
    pub fn dimensions(&self) -> Option<String> {
        match (self.width, self.height) {
            (Some(width), Some(height)) => Some(format!("{width}×{height}")),
            _ => None,
        }
    }

    /// Compact single-line description for composers and transcripts. Carries
    /// metadata only, never encoded bytes.
    #[must_use]
    pub fn compact_label(&self) -> String {
        match self.dimensions() {
            Some(dimensions) => format!("[{}: {} · {dimensions}]", self.kind.label(), self.label()),
            None => format!("[{}: {}]", self.kind.label(), self.label()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub output: String,
    pub is_error: bool,
    pub artifact_id: Option<String>,
    /// Images the tool produced (for example `read_image`). Empty for every
    /// existing text-only tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaRef>,
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

/// Durable identity of one root-scoped agent group. The group is a
/// coordination overlay: it owns shared tasks, claims, and a peer mailbox, but
/// never owns the child sessions themselves. Children remain nodes of the
/// existing agent graph and are executed by the existing supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGroupIdentity {
    pub group_id: Uuid,
    pub root_session_id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

/// Lifecycle of one shared group task.
///
/// `Pending` is unowned and claimable once every dependency is `Completed`.
/// `Claimed` means one agent atomically owns the task but has not started.
/// `InProgress` is the assignee's "I am working on this now" state. `Blocked`
/// and `Cancelled` are explicit non-completions: neither ever satisfies a
/// dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupTaskStatus {
    Pending,
    Claimed,
    InProgress,
    Completed,
    Blocked,
    Cancelled,
}

impl GroupTaskStatus {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }

    /// Terminal states never change again in this version.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }

    /// States an assignee is actively holding.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Claimed | Self::InProgress)
    }
}

impl std::fmt::Display for GroupTaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One durable shared work item in an agent group. The group task is
/// coordination state, not root evidence: completing it never certifies a
/// claim in the root evidence ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupTask {
    pub task_id: Uuid,
    pub group_id: Uuid,
    pub title: String,
    pub description: String,
    pub status: GroupTaskStatus,
    /// Task ids that must be `Completed` before this task is ready. The graph is
    /// validated as a real DAG at creation time.
    #[serde(default)]
    pub dependencies: Vec<Uuid>,
    /// Set while a task is claimed/in progress; `None` for a released or
    /// never-claimed task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Uuid>,
    /// Required tasks gate root terminal completion. Optional tasks inform the
    /// plan without blocking it.
    #[serde(default = "default_true")]
    pub required: bool,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Concise completion summary recorded by the assignee on `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Semantic findings reported on completion. These are coordination notes,
    /// never evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<String>,
    /// Paths the task expects to touch, used only for advisory conflict
    /// warnings between concurrently active tasks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_paths: Vec<String>,
    /// Paths reported touched on completion, used only for advisory conflict
    /// warnings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touched_files: Vec<String>,
    /// Why a blocked or cancelled task is not complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl GroupTask {
    /// A task is ready exactly when it is pending, unowned, and every
    /// dependency is truly completed.
    #[must_use]
    pub fn is_ready(&self, tasks: &std::collections::BTreeMap<Uuid, GroupTask>) -> bool {
        self.dependencies.iter().all(|dependency| {
            tasks
                .get(dependency)
                .is_some_and(|task| task.status == GroupTaskStatus::Completed)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum GroupMessageTarget {
    /// One specific agent (root or child session id).
    Agent(Uuid),
    /// The root session.
    Root,
    /// Every current group member plus the root.
    Group,
}

/// Compact durable peer message. Messages are information, not evidence, and
/// never copy transcripts: only the text and its routing metadata are durable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMessage {
    pub message_id: Uuid,
    pub group_id: Uuid,
    /// Sender session id (the root session or a child session).
    pub from_agent: Uuid,
    pub to: GroupMessageTarget,
    pub text: String,
    pub created_at: DateTime<Utc>,
}

fn default_true() -> bool {
    true
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A provider-visible text part that carried an opaque signature (for
    /// example Gemini's final-part `thoughtSignature`). `text` is the exact
    /// part text; the artifact's position in the sequence is significant.
    SignedText {
        #[serde(default)]
        text: String,
        #[serde(default)]
        signature: String,
    },
    /// Positional marker for a tool-call part that carries opaque replay state
    /// (for example a Gemini `functionCall` part with a `thoughtSignature`).
    /// The call itself stays in `tool_calls` and is referenced by its durable
    /// id, so parallel and same-name calls remain distinguishable while the
    /// exact part order is preserved.
    ToolCall {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        signature: String,
    },
}

impl std::fmt::Debug for ReasoningArtifact {
    /// Debug is redacted: encrypted and redacted payloads are opaque provider
    /// state and a signature authenticates a block, so logs must never expose
    /// them. Sizes stay visible for diagnostics.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text { text } => f.debug_struct("Text").field("text", text).finish(),
            Self::Encrypted { data } => f
                .debug_struct("Encrypted")
                .field("data", &format_args!("<redacted {} bytes>", data.len()))
                .finish(),
            Self::Thinking { text, signature } => f
                .debug_struct("Thinking")
                .field("text", text)
                .field(
                    "signature",
                    &format_args!("<redacted {} bytes>", signature.len()),
                )
                .finish(),
            Self::Redacted { data } => f
                .debug_struct("Redacted")
                .field("data", &format_args!("<redacted {} bytes>", data.len()))
                .finish(),
            Self::SignedText { text, signature } => f
                .debug_struct("SignedText")
                .field("text", text)
                .field(
                    "signature",
                    &format_args!("<redacted {} bytes>", signature.len()),
                )
                .finish(),
            Self::ToolCall { call_id, signature } => f
                .debug_struct("ToolCall")
                .field("call_id", call_id)
                .field(
                    "signature",
                    &format_args!("<redacted {} bytes>", signature.len()),
                )
                .finish(),
        }
    }
}

impl ReasoningArtifact {
    /// Tokens this artifact contributes to a replayed request, priced by the
    /// caller's estimator over its provider-visible payload.
    #[must_use]
    pub fn replay_text(&self) -> &str {
        match self {
            Self::Text { text } | Self::Thinking { text, .. } | Self::SignedText { text, .. } => {
                text
            }
            Self::Encrypted { data } | Self::Redacted { data } => data,
            // A tool-call marker's payload is the call arguments, accounted
            // separately from reasoning replay.
            Self::ToolCall { .. } => "",
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
    /// The root session's durable agent group was created. Group state is a
    /// coordination overlay: every other group event below is a plain durable
    /// fact, and any projection (task table, delivery markers) is rebuildable
    /// from these events alone.
    AgentGroupCreated {
        identity: AgentGroupIdentity,
    },
    /// A child explicitly joined the group. Appended once per agent; joining is
    /// idempotent and survives resume.
    AgentGroupMemberJoined {
        group_id: Uuid,
        agent_id: Uuid,
    },
    GroupTaskCreated {
        task: GroupTask,
    },
    /// One agent won the atomic claim of a task. Exactly one such event exists
    /// per claim; the task row is updated in the same store transaction.
    GroupTaskClaimed {
        task_id: Uuid,
        agent_id: Uuid,
    },
    /// Any other task state transition (`start`, `complete`, `block`,
    /// `cancel`, or an administrative reassignment). `actor` is the session id
    /// that made the transition; `assignee` is present only when the
    /// transition changes ownership (reassignment).
    GroupTaskStatusChanged {
        task_id: Uuid,
        status: GroupTaskStatus,
        actor: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignee: Option<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        findings: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        touched_files: Vec<String>,
    },
    /// The assignee explicitly returned a task to the pending pool.
    GroupTaskReleased {
        task_id: Uuid,
        agent_id: Uuid,
    },
    /// A peer message was durably queued in the root log. Queuing is not
    /// delivery: the message enters an agent's provider-visible history only
    /// when `GroupMessageDelivered` is appended to that agent's own session at
    /// a safe model boundary.
    GroupMessageQueued {
        message: GroupMessage,
    },
    /// A queued message was delivered to one recipient. Appended to the
    /// recipient's own session, so it is both the provider-visible turn and the
    /// exactly-once resume receipt.
    GroupMessageDelivered {
        message: GroupMessage,
    },
    UserMessage {
        text: String,
        /// Images attached by the user. Empty for text-only traffic, which is
        /// therefore structurally unchanged on the wire and in SQLite.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        media: Vec<MediaRef>,
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
    /// A write-capable operation is about to run. It advances verification's
    /// workspace generation before side effects, even when drift detection
    /// cannot later enumerate the changed paths.
    WorkspaceMutationPossible {
        operation: String,
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
        /// Missing on older events; replay treats those processes as writable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        may_write_workspace: Option<bool>,
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
    /// Global insertion order of the latest durable mutation for this
    /// workspace when kernel validation produced this observation. Legacy
    /// evidence has no version and cannot verify until revalidated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_generation: Option<u64>,
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
    /// Images visible in the current provider-visible epoch. Counts image
    /// inputs, including images returned by tools.
    #[serde(default)]
    pub image_count: usize,
    /// Estimated visual/input tokens for those images. A partition of
    /// `recent_tokens` (and therefore of `cache_epoch_tokens`), never added to
    /// the total separately. Provider-reported usage remains authoritative.
    #[serde(default)]
    pub image_tokens: usize,
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
    /// Images carried by this message. The references are provider-neutral and
    /// content-addressed; provider adapters resolve them to inline bytes at
    /// the wire boundary. Text-only messages keep this empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaRef>,
    /// Kernel tool outcome for a `role: "tool"` message: true when the durable
    /// record is `ToolFailed` rather than `ToolCompleted`. The kernel keeps the
    /// distinction typed across this boundary so the model never has to infer
    /// failure from the wording of arbitrary command output. Providers with a
    /// native tool-result error signal map it directly; providers without one
    /// carry it in the message content. Always false for non-tool roles.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

/// `skip_serializing_if` predicate: a false tool-error flag stays off the wire
/// and out of the canonical request signature, so only real failures cost
/// bytes. Legacy messages that predate the field deserialize to `false`.
fn is_false(value: &bool) -> bool {
    !*value
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
            media: Vec::new(),
            is_error: false,
        }
    }

    /// A `role: "tool"` result message. `is_error` is the kernel's typed tool
    /// outcome, never a guess derived from the output text.
    #[must_use]
    pub fn tool_result(
        call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
        media: Vec<MediaRef>,
    ) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: Some(call_id.into()),
            reasoning_content: None,
            reasoning: Vec::new(),
            media,
            is_error,
        }
    }

    /// True when the message carries any media, regardless of kind.
    #[must_use]
    pub fn has_media(&self) -> bool {
        !self.media.is_empty()
    }
}

/// One structured user input: text plus zero or more durable image references.
/// Used for the initial prompt and for mid-run steering, so image input is not
/// limited to the first request of a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaRef>,
}

impl UserInput {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            media: Vec::new(),
        }
    }

    #[must_use]
    pub fn new(text: impl Into<String>, media: Vec<MediaRef>) -> Self {
        Self {
            text: text.into(),
            media,
        }
    }

    #[must_use]
    pub fn has_media(&self) -> bool {
        !self.media.is_empty()
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self::text(text)
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
        /// Attached images in compact metadata form. The transcript never
        /// renders bytes, only names and dimensions.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        media: Vec<MediaRef>,
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
        EventPayload::UserMessage { text, media } => vec![DisplayItem::UserMessage {
            text: text.clone(),
            media: media.clone(),
        }],
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
            ReasoningArtifact::SignedText {
                text: "visible answer".into(),
                signature: "signed-text".into(),
            },
            ReasoningArtifact::ToolCall {
                call_id: "call-7".into(),
                signature: "signed-call".into(),
            },
        ];
        let json = serde_json::to_string(&artifacts).unwrap();
        assert!(json.contains("\"kind\":\"thinking\""));
        assert!(json.contains("\"kind\":\"redacted\""));
        assert!(json.contains("\"kind\":\"signed_text\""));
        assert!(json.contains("\"kind\":\"tool_call\""));
        let back: Vec<ReasoningArtifact> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, artifacts);
        assert_eq!(artifacts[0].replay_text(), "deepseek reasoning");
        assert_eq!(artifacts[3].replay_text(), "redacted");
        assert_eq!(artifacts[4].replay_text(), "visible answer");
        assert_eq!(artifacts[5].replay_text(), "");
    }

    #[test]
    fn positional_replay_artifacts_survive_the_durable_event_representation() {
        // The exact ordered sequence of thought, signed-text, and tool-call
        // parts must survive event serialization without reordering.
        let payload = EventPayload::AssistantMessageCompleted {
            text: "answer".into(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "a"}),
            }],
            reasoning_content: None,
            reasoning: vec![
                ReasoningArtifact::Thinking {
                    text: "thought one".into(),
                    signature: "sig-one".into(),
                },
                ReasoningArtifact::ToolCall {
                    call_id: "call-1".into(),
                    signature: "sig-call".into(),
                },
                ReasoningArtifact::SignedText {
                    text: "answer".into(),
                    signature: "sig-final".into(),
                },
            ],
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: EventPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, payload);

        // Signatures and thought text are internal replay state: normal UI
        // rendering only shows the assistant text.
        let event = Event {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
            timestamp: chrono::Utc::now(),
            parent_id: None,
            payload: payload.clone(),
        };
        let rendered = format!("{:?}", display_items(&event));
        assert!(rendered.contains("answer"), "{rendered}");
        for hidden in ["thought one", "sig-one", "sig-call", "sig-final"] {
            assert!(!rendered.contains(hidden), "UI leaked {hidden}: {rendered}");
        }

        // Old durable events without the new variants deserialize unchanged.
        let legacy: EventPayload = serde_json::from_str(
            r#"{"type":"assistant_message_completed","data":{"text":"hi","tool_calls":[],"reasoning_content":null}}"#,
        )
        .unwrap();
        let EventPayload::AssistantMessageCompleted { reasoning, .. } = legacy else {
            panic!("expected assistant message");
        };
        assert!(reasoning.is_empty());
    }

    #[test]
    fn opaque_reasoning_artifacts_have_redacted_debug_output() {
        let encrypted = ReasoningArtifact::Encrypted {
            data: "opaque-ciphertext".into(),
        };
        let redacted = ReasoningArtifact::Redacted {
            data: "redacted-ciphertext".into(),
        };
        let thinking = ReasoningArtifact::Thinking {
            text: "summary".into(),
            signature: "opaque-signature".into(),
        };
        let signed = ReasoningArtifact::SignedText {
            text: "answer".into(),
            signature: "signed-secret".into(),
        };
        let call = ReasoningArtifact::ToolCall {
            call_id: "call-1".into(),
            signature: "call-secret".into(),
        };
        for (artifact, secret) in [
            (&encrypted, "opaque-ciphertext"),
            (&redacted, "redacted-ciphertext"),
            (&thinking, "opaque-signature"),
            (&signed, "signed-secret"),
            (&call, "call-secret"),
        ] {
            let debug = format!("{artifact:?}");
            assert!(
                !debug.contains(secret),
                "debug output leaked opaque provider data: {debug}"
            );
            assert!(debug.contains("redacted"), "debug output: {debug}");
        }
        // Readable reasoning text stays diagnosable.
        let text = ReasoningArtifact::Text {
            text: "visible reasoning".into(),
        };
        assert!(format!("{text:?}").contains("visible reasoning"));
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

    fn image_ref() -> MediaRef {
        MediaRef {
            id: "abc123".into(),
            kind: MediaKind::Image,
            mime_type: "image/png".into(),
            artifact_path: "media/abc123.png".into(),
            sha256: "abc123".into(),
            byte_len: 2048,
            width: Some(1440),
            height: Some(900),
            display_name: Some("screenshot.png".into()),
        }
    }

    #[test]
    fn media_ref_round_trips_without_encoded_bytes() {
        let media = image_ref();
        let json = serde_json::to_string(&media).unwrap();
        assert!(json.contains("\"kind\":\"image\""));
        assert!(json.contains("\"artifact_path\":\"media/abc123.png\""));
        assert!(
            !json.contains("base64") && !json.contains("data:image"),
            "durable media metadata must never carry encoded bytes: {json}"
        );
        let back: MediaRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, media);
        assert_eq!(media.label(), "screenshot.png");
        assert_eq!(media.dimensions().as_deref(), Some("1440×900"));
        assert_eq!(media.compact_label(), "[image: screenshot.png · 1440×900]");
        // Debug output is metadata-only and cannot leak image content.
        let debug = format!("{media:?}");
        assert!(debug.contains("screenshot.png"));
        assert!(!debug.contains("2048:"));
    }

    #[test]
    fn legacy_text_only_events_deserialize_with_empty_media() {
        // Exact legacy shape: no `media` key at all.
        let legacy_user = r#"{"type":"user_message","data":{"text":"fix it"}}"#;
        let payload: EventPayload = serde_json::from_str(legacy_user).unwrap();
        match payload {
            EventPayload::UserMessage { text, media } => {
                assert_eq!(text, "fix it");
                assert!(media.is_empty());
            }
            other => panic!("unexpected payload {other:?}"),
        }
        let legacy_tool = r#"{"type":"tool_completed","data":{"result":{"call_id":"c1","name":"read_file","output":"ok","is_error":false,"artifact_id":null}}}"#;
        let payload: EventPayload = serde_json::from_str(legacy_tool).unwrap();
        match payload {
            EventPayload::ToolCompleted { result } => {
                assert!(result.media.is_empty());
            }
            other => panic!("unexpected payload {other:?}"),
        }
        // Text-only serialization stays structurally cheap: no empty `media`
        // arrays are written.
        let payload = EventPayload::UserMessage {
            text: "plain".into(),
            media: vec![],
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("media"), "{json}");
        let message = ModelMessage::text("user", "plain");
        let json = serde_json::to_string(&message).unwrap();
        assert!(!json.contains("media"), "{json}");
        assert!(!message.has_media());
        let result = ToolResult {
            call_id: "c".into(),
            name: "read_file".into(),
            output: "ok".into(),
            is_error: false,
            artifact_id: None,
            media: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("media"), "{json}");
    }

    #[test]
    fn media_flows_through_messages_tool_results_and_display_items() {
        let media = vec![image_ref()];
        let message = ModelMessage {
            role: "user".into(),
            is_error: false,
            content: "look".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning: vec![],
            media: media.clone(),
        };
        assert!(message.has_media());
        let round: ModelMessage =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        assert_eq!(round, message);

        let result = ToolResult {
            call_id: "call-1".into(),
            name: "read_image".into(),
            output: "image.png".into(),
            is_error: false,
            artifact_id: None,
            media: media.clone(),
        };
        let event = Event {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            sequence: 1,
            timestamp: Utc::now(),
            parent_id: None,
            payload: EventPayload::ToolCompleted {
                result: result.clone(),
            },
        };
        let round: Event = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(round, event);
        let items = display_items(&round);
        assert!(
            matches!(&items[0], DisplayItem::ToolActivity { verb, .. } if verb == "read_image")
        );

        let user = Event {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            sequence: 2,
            timestamp: Utc::now(),
            parent_id: None,
            payload: EventPayload::UserMessage {
                text: "inspect".into(),
                media,
            },
        };
        let items = display_items(&user);
        match &items[0] {
            DisplayItem::UserMessage { text, media } => {
                assert_eq!(text, "inspect");
                assert_eq!(
                    media[0].compact_label(),
                    "[image: screenshot.png · 1440×900]"
                );
            }
            other => panic!("unexpected item {other:?}"),
        }
    }

    #[test]
    fn input_modalities_are_provider_neutral_and_parse_from_config() {
        assert_eq!(
            serde_json::to_string(&InputModality::Text).unwrap(),
            "\"text\""
        );
        assert_eq!(
            serde_json::to_string(&InputModality::Image).unwrap(),
            "\"image\""
        );
        let modalities: Vec<InputModality> = serde_json::from_str(r#"["text","image"]"#).unwrap();
        assert_eq!(modalities, vec![InputModality::Text, InputModality::Image]);
        assert!(serde_json::from_str::<InputModality>("\"audio\"").is_err());
    }

    #[test]
    fn group_types_round_trip_deterministically() {
        let group_id = Uuid::new_v4();
        let root = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let task = GroupTask {
            task_id: Uuid::new_v4(),
            group_id,
            title: "parser".into(),
            description: "implement the parser".into(),
            status: GroupTaskStatus::Claimed,
            dependencies: vec![Uuid::new_v4()],
            assignee: Some(agent),
            required: true,
            created_by: root,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            summary: None,
            findings: vec![],
            expected_paths: vec!["src/parser.rs".into()],
            touched_files: vec![],
            reason: None,
        };
        let payloads = vec![
            EventPayload::AgentGroupCreated {
                identity: AgentGroupIdentity {
                    group_id,
                    root_session_id: root,
                    name: "workspace".into(),
                    created_at: Utc::now(),
                },
            },
            EventPayload::AgentGroupMemberJoined {
                group_id,
                agent_id: agent,
            },
            EventPayload::GroupTaskCreated { task: task.clone() },
            EventPayload::GroupTaskClaimed {
                task_id: task.task_id,
                agent_id: agent,
            },
            EventPayload::GroupTaskStatusChanged {
                task_id: task.task_id,
                status: GroupTaskStatus::Completed,
                actor: agent,
                assignee: None,
                summary: Some("done".into()),
                reason: None,
                findings: vec!["used a precedence table".into()],
                touched_files: vec!["src/parser.rs".into()],
            },
            EventPayload::GroupTaskReleased {
                task_id: task.task_id,
                agent_id: agent,
            },
            EventPayload::GroupMessageQueued {
                message: GroupMessage {
                    message_id: Uuid::new_v4(),
                    group_id,
                    from_agent: agent,
                    to: GroupMessageTarget::Root,
                    text: "parser done".into(),
                    created_at: Utc::now(),
                },
            },
        ];
        for payload in payloads {
            let event = Event {
                id: Uuid::new_v4(),
                session_id: root,
                sequence: 1,
                timestamp: Utc::now(),
                parent_id: None,
                payload,
            };
            let encoded = serde_json::to_string(&event).unwrap();
            let round: Event = serde_json::from_str(&encoded).unwrap();
            assert_eq!(round, event);
            // A second serialization is byte-identical: no map iteration or
            // formatting nondeterminism may leak into durable events.
            assert_eq!(serde_json::to_string(&round).unwrap(), encoded);
        }
    }

    #[test]
    fn older_events_deserialize_with_no_group_state() {
        // Any historical payload still deserializes; group events simply do not
        // exist in old logs, and the new task fields default.
        let session = Uuid::new_v4();
        let legacy: EventPayload = serde_json::from_str(
            &serde_json::to_string(&EventPayload::UserMessage {
                text: "legacy".into(),
                media: vec![],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(legacy, EventPayload::UserMessage { .. }));
        let task: GroupTask = serde_json::from_value(serde_json::json!({
            "task_id": Uuid::new_v4(),
            "group_id": Uuid::new_v4(),
            "title": "t",
            "description": "d",
            "status": "pending",
            "dependencies": [],
            "created_by": session,
            "created_at": Utc::now(),
            "updated_at": Utc::now(),
        }))
        .unwrap();
        assert!(task.required, "required defaults to true for old payloads");
        assert!(task.assignee.is_none());
    }
}
