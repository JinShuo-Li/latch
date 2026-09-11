#![forbid(unsafe_code)]

pub mod agent;
pub mod config;
pub mod continuity;
pub mod extension;
pub mod prompt;
pub mod provider;
pub mod state;
pub mod store;
pub mod tools;

pub use agent::{Agent, AgentEventSink};
pub use config::Config;
pub use continuity::{ContinuityEngine, MaterializedContext};
pub use provider::{AnthropicProvider, FakeProvider, ModelProvider, OpenAiProvider};
pub use state::{EvidenceLedger, FailureManager, TaskStateManager};
pub use store::EventStore;
pub use tools::{PolicyEngine, ToolExecutor};
