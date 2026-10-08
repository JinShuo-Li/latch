//! Provider-neutral configuration display contracts.
use crate::profile::EffortMapEdit;
use latch_protocol::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    Ready,
    MissingCredential,
    UnresolvedModels,
}

impl ProviderStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::MissingCredential => "missing credential",
            Self::UnresolvedModels => "unresolved models",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ProviderSummary {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    pub status: ProviderStatus,
    pub model_count: usize,
    pub default_model: String,
    pub credential_ref: String,
    /// Effective base URL the transport will use.
    pub base_url: String,
    /// Enabled models with resolved transport, eligible as provider defaults.
    pub available_models: Vec<String>,
    pub models: Vec<ProviderModelSummary>,
    pub discovered_ids: Vec<String>,
}

impl ProviderSummary {
    pub fn merge_discovered(&mut self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        self.discovered_ids = ids.to_vec();
        for id in ids {
            if !self.models.iter().any(|model| model.id == *id) {
                self.models.push(ProviderModelSummary {
                    id: id.clone(),
                    display_name: id.clone(),
                    enabled: false,
                    resolved: false,
                    from_catalog: false,
                    ..ProviderModelSummary::default()
                });
            }
        }
    }
}

/// One model row as `/setup` needs to display and edit it. Capability display
/// is provider-neutral: no wire parameters or secrets.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ProviderModelSummary {
    pub id: String,
    pub display_name: String,
    pub enabled: bool,
    pub resolved: bool,
    /// True when the built-in catalog (not the user) knows this id.
    pub from_catalog: bool,
    /// Effective transport label.
    pub transport: String,
    /// Whether the user explicitly set the transport.
    pub transport_configured: bool,
    pub context_window_tokens: Option<usize>,
    /// Explicit replay override, when set by the user.
    pub reasoning_replay: Option<String>,
    pub adaptive_thinking: bool,
    pub adaptive_thinking_configured: bool,
    /// Effective Gemini thinking capability summary from the CLI, when the
    /// model declares one. Display text only; no wire names.
    pub gemini_thinking: Option<String>,
    pub gemini_thinking_configured: bool,
    pub input_modalities: Vec<String>,
    pub input_modalities_configured: bool,
    pub aliases: Vec<String>,
    pub aliases_configured: bool,
    /// Effective exposed efforts, already resolved through catalog + override.
    pub efforts: Vec<ReasoningEffort>,
    pub default_effort: ReasoningEffort,
    /// True when the user has any sparse override for this model.
    pub has_override: bool,
    /// Current user effort map (empty means the adapter default).
    pub effort_map: BTreeMap<ReasoningEffort, EffortMapEdit>,
    /// Current user pricing override, when set.
    pub pricing: Option<latch_protocol::ModelPricing>,
    /// Catalog-suggested transport (for showing "provider default" vs override).
    pub catalog_transport: String,
}

impl Default for ProviderModelSummary {
    fn default() -> Self {
        Self {
            id: String::new(),
            display_name: String::new(),
            enabled: false,
            resolved: false,
            from_catalog: false,
            transport: String::new(),
            transport_configured: false,
            context_window_tokens: None,
            reasoning_replay: None,
            adaptive_thinking: false,
            adaptive_thinking_configured: false,
            gemini_thinking: None,
            gemini_thinking_configured: false,
            input_modalities: Vec::new(),
            input_modalities_configured: false,
            aliases: Vec::new(),
            aliases_configured: false,
            efforts: Vec::new(),
            default_effort: ReasoningEffort::ProviderDefault,
            has_override: false,
            effort_map: BTreeMap::new(),
            pricing: None,
            catalog_transport: String::new(),
        }
    }
}
