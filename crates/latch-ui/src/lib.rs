#![forbid(unsafe_code)]
//! Shared interface contracts. No terminal, HTTP, or provider wire dependencies.
pub mod configuration;
pub mod profile;
pub use configuration::*;
use latch_protocol::ModelPricing as Pricing;
use latch_protocol::{
    Event as DurableEvent, MediaRef, Mode, PermissionMode, ReasoningEffort, Safety, ToolResult,
};
pub use profile::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Input {
    Submit {
        text: String,
        media: Vec<MediaRef>,
    },
    /// Ask the kernel to validate and ingest one image path as a pending
    /// attachment. The kernel owns artifact ingestion; the TUI never reads
    /// image bytes itself.
    Attach(String),
    Cancel,
    Resume,
    Quit,
    /// A real human decision for a kernel approval request.
    Permission {
        request_id: uuid::Uuid,
        approved: bool,
    },
    /// A selector choice for the durable safety profile.
    SetSafety(Safety),
    /// A selector choice for the durable permission resolver.
    SetPermissions(PermissionMode),
    /// The live inference-profile selector chose a provider/model/effort.
    SetInferenceProfile {
        provider: String,
        model: String,
        effort: ReasoningEffort,
    },
    /// The `/setup` flow completed and should be persisted and applied.
    SetupApply(SetupPlan),
    DiscoverModels {
        provider: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Output {
    /// Controller is idle, including after an errored or cancelled turn.
    Ready,
    /// An accepted browser submission reached the controller. This is a
    /// transport coordination receipt, not a durable completion assertion.
    InputReceived,
    AssistantDelta(String),
    AssistantDone,
    /// One user-visible transcript element from the shared durable-event
    /// formatter. Used for live kernel events and resume replay alike.
    Event(Box<DurableEvent>),
    ToolResult(ToolResult),
    Notice(String),
    Mode(Mode),
    /// Effective safety profile after a `/safety` change or resume.
    Safety(Safety),
    /// Effective permission resolver after a `/permissions` change or resume.
    Permissions(PermissionMode),
    Header {
        model: String,
        /// Friendly provider label, for example `OpenCode Go` or `Anthropic`.
        provider: String,
        /// Stable configured provider id, for example `opencode-go`.
        provider_id: String,
        /// Effective reasoning effort of the session.
        effort: ReasoningEffort,
        /// Session workspace, shown in the welcome state and composer footer.
        workspace: String,
        branch: String,
        resumed: bool,
        /// Optional user-configured pricing for the session model. `None` means
        /// the sidebar must show estimated cost as unavailable.
        pricing: Option<Pricing>,
    },
    /// A workspace diff to open in the full-width inspector.
    Diff(String),
    /// Submitted prompts from the durable session, seeding prompt history on
    /// resume without a second history database.
    History(Vec<String>),
    /// One validated image attachment, ingested by the kernel into immutable
    /// artifact storage. The composer shows it as a pending attachment.
    Attachment(MediaRef),
    /// Provider-neutral catalog for the live `/model` selector.
    InferenceCatalog(InferenceCatalog),
    /// Provider kinds and built-in models available to `/setup`.
    SetupCatalog(Vec<SetupKind>),
    SetupProviders(Vec<ProviderSummary>),
    SetupModels {
        provider: String,
        ids: Vec<String>,
    },
    /// Resolved storage the configuration center will write to. Shown on
    /// review; never a secret.
    SetupPaths(SetupPaths),
    /// The provider cannot run until setup completes; open the guided flow.
    SetupRequired,
    /// The effective profile changed; update model/effort chrome.
    Inference {
        provider_id: String,
        provider_label: String,
        model: String,
        effort: ReasoningEffort,
    },
    /// A symbolic credential reference for a configured provider, used by
    /// `/setup` to prefill without ever exposing a value.
    CredentialLabel {
        provider_id: String,
        label: String,
    },
}

/// Resolved storage paths shown by `/setup` review surfaces. Values are paths
/// and a symbolic source label only; no credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPaths {
    pub config_path: String,
    pub state_root: String,
    pub source: String,
}

pub mod commands;

pub mod agents;
pub mod diff;
pub mod group;
pub mod presentation;

pub mod sidebar;
