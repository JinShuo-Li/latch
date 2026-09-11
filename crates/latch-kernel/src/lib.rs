#![forbid(unsafe_code)]

pub mod agent;
pub mod config;
pub mod continuity;
pub mod extension;
pub mod linediff;
pub mod permissions;
pub mod progress;
pub mod prompt;
pub mod provider;
pub mod session;
pub mod state;
pub mod store;
pub mod tokens;
pub mod tools;

pub use agent::{Agent, AgentEventSink, AgentRuntime};
pub use config::Config;
pub use continuity::{ContinuityEngine, MaterializeBudget, MaterializedContext};
pub use permissions::PermissionBroker;
pub use progress::{ProgressSupervisor, StagnationDecision};
pub use provider::{AnthropicProvider, FakeProvider, ModelProvider, OpenAiProvider};
pub use state::{EvidenceLedger, FailureManager, TaskStateManager};
pub use store::{EventStore, SessionPreviewLine, SessionSummary};
pub use tokens::{TokenEstimator, TokenProfile};
pub use tools::{PolicyEngine, ToolExecutor};
