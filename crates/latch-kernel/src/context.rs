//! The context-engine port: the narrow contract between the agent runtime and
//! whatever decides what the model is allowed to see next.
//!
//! A [`ContextEngine`] owns exactly one invariant: for one request it returns a
//! bounded, provider-neutral [`ContextView`] whose `recent` events are the
//! provider-visible history of the current durable cache epoch. Latch's
//! [`crate::continuity::ContinuityEngine`] is the reference implementation:
//! L0-L3 memory, canonical state, FTS recall, episodes, cache epochs,
//! hysteretic rotation, `/compact`, and resume equivalence all live there.
//!
//! Port rules (normative text in `docs/RUNTIME_CAPABILITY_MODEL.md`):
//!
//! - **Requests and views only.** The contract never exposes an `EventStore`,
//!   SQLite handle, or the agent loop, so a different engine (a remote context
//!   service, a smaller local policy) can be implemented without reaching into
//!   kernel internals.
//! - **Durable, provider-neutral results.** `ContextView::recent` is a list of
//!   durable protocol events. The agent derives provider messages from them;
//!   an engine never touches provider wire formats.
//! - **Kernel authority is not transferable.** Implementing this port means
//!   running with kernel authority (the default implementation appends
//!   `KernelContext`/`ContextEpochStarted` events itself). Implementations are
//!   trusted, operator-installed components, never model-supplied or
//!   extension-supplied code. Extensions and remote clients reach context only
//!   through kernel-mediated surfaces.
//! - **Pure accessor semantics.** [`ContextEngine::materialize`] takes `&self`
//!   and returns the view; only explicit lifecycle operations
//!   ([`ContextEngine::set_config`], [`ContextEngine::set_estimator`],
//!   [`ContextEngine::manual_compact`]) take `&mut self`.

use crate::config::ContextConfig;
use crate::state::{EvidenceLedger, FailureManager};
use crate::tokens::TokenEstimator;
use anyhow::Result;
use latch_protocol::Event;
use latch_protocol::{ContextStats, InferenceProfile, TaskState};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// One scored archival episode: a bounded, navigational summary over an intent
/// aligned event range. Descriptions point at raw durable sources and are never
/// a substitute for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    pub start_sequence: u64,
    pub end_sequence: u64,
    pub topic: String,
    /// Workspace paths touched or read inside the episode.
    pub entities: Vec<String>,
    /// Tool names executed inside the episode.
    pub tools: Vec<String>,
    /// Structural markers such as `validation-passed`, `validation-failed`,
    /// `reground`, `mutation`, `evidence`, `failure`.
    pub markers: Vec<String>,
    pub summary: String,
}

/// Navigation-only bridge between the current instruction and canonical state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationBridge {
    pub current_topic: String,
    pub current_user_intent: String,
    pub unresolved_references: Vec<String>,
    pub recent_decisions: Vec<String>,
    pub ongoing_action: Option<String>,
}

/// Token budget for one materialized request view: what the whole request may
/// spend, what the model window and reserve are, and what the caller already
/// reserved for tool schemas and extension context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// Complete request budget (`context window - reserve`).
    pub request_tokens: usize,
    /// The model's full context window, for display.
    pub window_tokens: usize,
    /// Tokens reserved for the model response plus safety.
    pub reserve_tokens: usize,
    /// Upper bound for the verbatim recent transcript.
    pub recent_tokens: usize,
    /// Tokens already reserved by the caller for tool schemas and extension
    /// context; the engine sizes its own sections inside the remainder.
    pub reserved_tokens: usize,
}

/// Everything one materialization needs, as an explicit request object.
///
/// The request borrows live kernel state; it carries no storage handle. A
/// future engine that cannot use `TaskState`/`EvidenceLedger`/`FailureManager`
/// directly still receives them as provider-neutral canonical input, and may
/// ignore what it does not model.
pub struct ContextRequest<'a> {
    pub session_id: Uuid,
    pub state: &'a TaskState,
    /// Retrieval query for this turn: a steer or the opening user intent.
    /// `None` means ordinary continuation and triggers no surprise recall.
    pub query: Option<&'a str>,
    pub evidence: &'a EvidenceLedger,
    pub failures: &'a FailureManager,
    /// Compiled session-independent system prompt for this request. It is
    /// identical across sessions, modes, and workspaces so the `system` + tools
    /// prefix stays reusable in the provider's prompt cache.
    pub system: String,
    /// Session-specific prompt content (workspace, repository instructions,
    /// and mode). The agent renders it as the first provider-visible message
    /// after the tools instead of inside `system`, so a session difference
    /// cannot invalidate the cacheable prefix.
    pub session_context: String,
    pub budget: ContextBudget,
    /// Serialized extension context sources for this turn, or `""`.
    pub extension_context: &'a str,
    /// Kernel re-ground instruction for this turn, or `None`.
    pub reground: Option<&'a str>,
}

/// One bounded, provider-visible request view.
///
/// `recent` is the authoritative provider-visible history: durable events in
/// epoch order that only ever append within a cache epoch. The remaining fields
/// are diagnostics for the kernel, the TUI, and tests; a different engine may
/// leave `episodes` empty and synthesize `bridge` defaults, but it must keep
/// `recent` protocol-valid and bounded.
pub struct ContextView {
    pub system: String,
    /// Session-specific prompt content carried as the first provider-visible
    /// message. See [`ContextRequest::session_context`].
    pub session_context: String,
    pub canonical: String,
    pub recalled: String,
    pub recent: Vec<Event>,
    pub bridge: ConversationBridge,
    /// The bounded, scored subset of episodes selected into the index.
    pub episodes: Vec<Episode>,
    pub stats: ContextStats,
}

/// Minimal, trusted inputs for constructing a child session's context engine.
///
/// The factory receives only what a context-engine policy genuinely needs: the
/// child's durable session id, its effective inference profile (so token
/// estimation, context windows, and budgeting stay aligned with the child's
/// actual model), and the session's context configuration. It never receives
/// an agent, mutable kernel state, or a storage handle; a policy that needs
/// durable history captures its own backend when it is constructed.
pub struct ContextEngineSpec<'a> {
    pub session_id: Uuid,
    pub profile: &'a InferenceProfile,
    pub context: &'a ContextConfig,
}

/// Builds the context engine for one child session under the root's
/// context-engine policy.
///
/// The root supervisor stores exactly one factory and calls it for every
/// spawn, worker reconstruction, and process resume, so root and children
/// share one explicit policy and a resumed child never silently falls back to
/// a different engine. The default factory is
/// [`crate::continuity::continuity_context_engine_factory`], which preserves
/// the kernel's historical `ContinuityEngine::for_model(...)` behavior.
pub type ContextEngineFactory =
    Arc<dyn Fn(&ContextEngineSpec<'_>) -> Result<Box<dyn ContextEngine>> + Send + Sync>;

/// Replaceable context engine: the mechanism that turns durable session state
/// into one bounded provider-visible view. See the module docs for the port
/// rules; `docs/RUNTIME_CAPABILITY_MODEL.md` maps it onto the kernel invariant
/// model.
pub trait ContextEngine: Send + Sync {
    /// Stable diagnostic name of the active implementation.
    fn name(&self) -> &str;

    /// The token configuration this engine is currently using.
    fn config(&self) -> &ContextConfig;

    /// Replaces the token configuration for subsequent requests.
    fn set_config(&mut self, config: ContextConfig);

    /// Keeps the engine's token pricing aligned with the effective provider
    /// model.
    fn set_estimator(&mut self, estimator: TokenEstimator);

    /// Derives the request budget for the effective model window and the
    /// tokens the caller already reserved.
    fn default_budget(&self, window_tokens: usize, reserved_tokens: usize) -> ContextBudget;

    /// Explicit working-memory reset (`/compact`): starts a fresh durable
    /// epoch. Never deletes raw history.
    fn manual_compact(&mut self, session_id: Uuid) -> Result<()>;

    /// Exact retrieval over durable history, independent of the current
    /// provider-visible epoch.
    fn recall(&self, session_id: Uuid, query: &str) -> Result<Vec<Event>>;

    /// Materializes one bounded request view.
    fn materialize(&self, request: ContextRequest<'_>) -> Result<ContextView>;
}

/// A boxed engine is still an engine. This lets the runtime pass a
/// factory-built `Box<dyn ContextEngine>` back through the generic
/// [`crate::agent::AgentRuntime`] construction path without erasing the port.
impl<T: ContextEngine + ?Sized> ContextEngine for Box<T> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn config(&self) -> &ContextConfig {
        (**self).config()
    }

    fn set_config(&mut self, config: ContextConfig) {
        (**self).set_config(config);
    }

    fn set_estimator(&mut self, estimator: TokenEstimator) {
        (**self).set_estimator(estimator);
    }

    fn default_budget(&self, window_tokens: usize, reserved_tokens: usize) -> ContextBudget {
        (**self).default_budget(window_tokens, reserved_tokens)
    }

    fn manual_compact(&mut self, session_id: Uuid) -> Result<()> {
        (**self).manual_compact(session_id)
    }

    fn recall(&self, session_id: Uuid, query: &str) -> Result<Vec<Event>> {
        (**self).recall(session_id, query)
    }

    fn materialize(&self, request: ContextRequest<'_>) -> Result<ContextView> {
        (**self).materialize(request)
    }
}
