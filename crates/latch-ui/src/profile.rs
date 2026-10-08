//! Provider/catalog-facing selector state machines for `/model` and `/setup`.
//!
//! The TUI never inspects base URLs, model families, or wire parameters. The
//! CLI sends a provider-neutral catalog; these machines turn keyboard input
//! into a profile selection or a setup plan.

use latch_protocol::{InputModality, ReasoningEffort};
use serde::{Deserialize, Serialize};

/// One selectable model as presented by the CLI catalog.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub id: String,
    pub display_name: String,
    pub efforts: Vec<ReasoningEffort>,
    pub default_effort: ReasoningEffort,
    /// Provider-neutral input modalities. Image input is explicit capability
    /// metadata, never inferred by the TUI from the model name.
    pub input_modalities: Vec<InputModality>,
}

impl CatalogModel {
    /// Selectable efforts, always including `provider default`.
    #[must_use]
    pub fn selectable_efforts(&self) -> Vec<ReasoningEffort> {
        let mut efforts = vec![ReasoningEffort::ProviderDefault];
        for effort in ReasoningEffort::LEVELS {
            if self.efforts.contains(&effort) {
                efforts.push(effort);
            }
        }
        efforts
    }

    /// The effort to highlight when this model is newly selected.
    #[must_use]
    pub fn preferred_effort(&self) -> ReasoningEffort {
        match self.default_effort {
            ReasoningEffort::ProviderDefault => ReasoningEffort::ProviderDefault,
            explicit if self.efforts.contains(&explicit) => explicit,
            _ => ReasoningEffort::ProviderDefault,
        }
    }

    /// Whether this model accepts image input.
    #[must_use]
    pub fn supports_image_input(&self) -> bool {
        self.input_modalities.contains(&InputModality::Image)
    }

    /// Restrained display label with a compact `vision` indicator for
    /// image-capable models.
    #[must_use]
    pub fn label(&self) -> String {
        let base = if self.display_name.trim().is_empty() {
            self.id.as_str()
        } else {
            self.display_name.as_str()
        };
        if self.supports_image_input() {
            format!("{base} · vision")
        } else {
            base.to_owned()
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CatalogProvider {
    pub id: String,
    pub display_name: String,
    /// Configured default model, used by the setup surface.
    pub default_model: String,
    pub models: Vec<CatalogModel>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct InferenceCatalog {
    pub providers: Vec<CatalogProvider>,
}

impl InferenceCatalog {
    pub fn provider_index(&self, id: &str) -> usize {
        self.providers.iter().position(|p| p.id == id).unwrap_or(0)
    }
}

/// One row rendered by a profile setup surface.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ChoiceRow {
    pub label: String,
    pub description: String,
    pub current: bool,
    pub selected: bool,
}

/// Provider kind offered by `/setup`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SetupKind {
    pub kind: String,
    pub label: String,
    pub default_base_url: String,
    pub requires_base_url: bool,
    pub credential_label: String,
    pub default_model: String,
    pub models: Vec<CatalogModel>,
}

/// One provider-level field edit. Only the named field changes; every other
/// provider setting and `[inference]` stay untouched.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ProviderFieldEdit {
    /// `None` restores the kind's built-in base URL.
    BaseUrl(Option<String>),
    /// `None` restores the kind's display name.
    DisplayName(Option<String>),
}

/// One model-level field edit. Only the named field changes; every other
/// override is preserved.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ModelFieldEdit {
    DisplayName(Option<String>),
    /// `None` restores the catalog transport.
    Transport(Option<String>),
    /// `None` restores the catalog/default window.
    ContextWindow(Option<usize>),
    /// `None` restores the catalog replay policy.
    ReasoningReplay(Option<String>),
    AdaptiveThinking(Option<bool>),
    /// `None` restores the catalog Gemini thinking capability.
    GeminiThinking(Option<GeminiThinkingEdit>),
    InputModalities(Vec<String>),
    Aliases(Vec<String>),
    Efforts {
        efforts: Vec<ReasoningEffort>,
        default_effort: Option<ReasoningEffort>,
    },
    /// An empty map clears the override and restores the adapter default.
    EffortMap(std::collections::BTreeMap<ReasoningEffort, EffortMapEdit>),
    /// Replaces the optional pricing override; `None` clears it.
    Pricing(Option<latch_protocol::ModelPricing>),
    /// Clear every override for this model (built-in catalog models only).
    Reset,
}

/// Provider-neutral Gemini thinking capability declaration edited by the
/// Advanced surface. The CLI translates it into kernel model metadata; the TUI
/// never sees wire field names.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum GeminiThinkingEdit {
    /// `thinkingLevel` controls over the declared levels; `off` is the level
    /// that expresses no thinking when the model has one.
    Levels {
        levels: Vec<String>,
        off: Option<String>,
    },
    /// `thinkingBudget` controls; `zero_allowed` is the documented off switch.
    Budget { zero_allowed: bool },
}

/// One effort level's edited wire form.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum EffortMapEdit {
    Automatic,
    Value(String),
    Budget(u64),
    Disabled,
}

/// A setup flow result that must leave the TUI.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum SetupPlan {
    Apply {
        /// Stable provider instance id (may differ from the kind id).
        name: String,
        provider_kind: String,
        base_url: Option<String>,
        credential: SetupCredential,
        model: String,
        /// Explicit model selection. `None` preserves an existing selection.
        enabled_models: Option<Vec<String>>,
        custom_model_display_name: Option<String>,
        custom_transport: Option<String>,
        effort: ReasoningEffort,
    },
    /// Remove one configured provider instance. Credentials are never deleted.
    Remove { name: String },
    /// Explicitly change the profile used by future sessions.
    SetNewSessionDefault { name: String },
    /// Change one provider's switch default without changing the live session.
    SetProviderDefault { name: String, model: String },
    /// Replace one provider's credential reference or securely stored value.
    SetCredential {
        name: String,
        credential: SetupCredential,
    },
    /// Replace the provider's enabled model set; metadata overrides are kept.
    SetEnabledModels { name: String, models: Vec<String> },
    /// Add a sparse custom-model entry; transport remains unresolved until
    /// Advanced sets one.
    AddCustomModel {
        name: String,
        model: String,
        display_name: String,
    },
    /// Edit exactly one provider field.
    SetProviderField {
        name: String,
        field: ProviderFieldEdit,
    },
    /// Edit exactly one model override field.
    SetModelField {
        name: String,
        model: String,
        field: ModelFieldEdit,
    },
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum SetupCredential {
    /// Reference an environment variable by name.
    Env(String),
    /// Enter a secret now; the CLI stores it in the 0600 local secrets file.
    Secret(String),
}

impl std::fmt::Debug for SetupCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env(name) => f.debug_tuple("Env").field(name).finish(),
            // A debug log must never expose a typed secret.
            Self::Secret(_) => f.debug_tuple("Secret").field(&"[redacted]").finish(),
        }
    }
}

/// Requested composer capture for a text field.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CaptureSpec {
    pub label: String,
    pub initial: String,
    pub masked: bool,
}

/// Output of one `confirm()` on a setup flow.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum SetupStepOutcome {
    None,
    Capture(CaptureSpec),
    Apply(SetupPlan),
    Cancel,
}
