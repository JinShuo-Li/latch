#![forbid(unsafe_code)]

pub mod agent;
pub mod agents;
pub mod config;
pub mod continuity;
pub mod credentials;
pub mod extension;
pub mod linediff;
pub mod permissions;
pub mod progress;
pub mod prompt;
pub mod provider;
pub mod providers;
pub mod safety;
pub mod sandbox;
pub mod session;
pub mod state;
pub mod store;
pub mod tokens;
pub mod tools;

pub use agent::{Agent, AgentEventSink, AgentRuntime};
pub use agents::{AgentSnapshot, AgentSupervisor};
pub use config::Config;
pub use continuity::{ContinuityEngine, MaterializeBudget, MaterializedContext};
pub use credentials::{CredentialRef, CredentialStore};
pub use permissions::PermissionBroker;
pub use progress::{ProgressSupervisor, StagnationDecision};
pub use provider::{
    AnthropicConfig, AnthropicProvider, FakeProvider, ModelProvider, OpenAiProvider,
    OpenAiResponsesProvider, ReasoningReplay, ThinkingToggle,
};
pub use providers::{ModelDescriptor, ProviderCapabilities, ProviderProfile, ProviderRegistry};
pub use state::{EvidenceLedger, FailureManager, TaskStateManager};
pub use store::{EventStore, SessionPreviewLine, SessionSummary};
pub use tokens::{TokenEstimator, TokenProfile};
pub use tools::{CapabilityGrant, PolicyEngine, ToolExecutor};
