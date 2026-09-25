//! Provider/catalog-facing selector state machines for `/model` and `/setup`.
//!
//! The TUI never inspects base URLs, model families, or wire parameters. The
//! CLI sends a provider-neutral catalog; these machines turn keyboard input
//! into a profile selection or a setup plan.

use latch_protocol::{InputModality, ReasoningEffort};

/// One selectable model as presented by the CLI catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogProvider {
    pub id: String,
    pub display_name: String,
    /// Configured default model, used by the setup surface.
    pub default_model: String,
    pub models: Vec<CatalogModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InferenceCatalog {
    pub providers: Vec<CatalogProvider>,
}

impl InferenceCatalog {
    fn provider_index(&self, id: &str) -> usize {
        self.providers.iter().position(|p| p.id == id).unwrap_or(0)
    }
}

/// One row rendered by a profile setup surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceRow {
    pub label: String,
    pub description: String,
    pub current: bool,
    pub selected: bool,
}

/// Provider kind offered by `/setup`.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderFieldEdit {
    /// `None` restores the kind's built-in base URL.
    BaseUrl(Option<String>),
    /// `None` restores the kind's display name.
    DisplayName(Option<String>),
}

/// One model-level field edit. Only the named field changes; every other
/// override is preserved.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelFieldEdit {
    DisplayName(Option<String>),
    /// `None` restores the catalog transport.
    Transport(Option<String>),
    /// `None` restores the catalog/default window.
    ContextWindow(Option<usize>),
    /// `None` restores the catalog replay policy.
    ReasoningReplay(Option<String>),
    AdaptiveThinking(Option<bool>),
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

/// One effort level's edited wire form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffortMapEdit {
    Automatic,
    Value(String),
    Budget(u64),
    Disabled,
}

/// A setup flow result that must leave the TUI.
#[derive(Debug, Clone, PartialEq)]
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

#[derive(Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSpec {
    pub label: String,
    pub initial: String,
    pub masked: bool,
}

/// Output of one `confirm()` on a setup flow.
#[derive(Debug, Clone, PartialEq)]
pub enum SetupStepOutcome {
    None,
    Capture(CaptureSpec),
    Apply(SetupPlan),
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfilePhase {
    Provider,
    Model,
    Effort,
}

/// `/model`: live provider model and effort selection.
#[derive(Debug, Clone)]
pub struct ProfileSelector {
    catalog: InferenceCatalog,
    phase: ProfilePhase,
    selected: usize,
    provider: usize,
    /// Explicitly selected model. `None` until the user highlights a concrete
    /// row; switching to a provider preselects its configured default model,
    /// never catalog index 0 merely because it is first.
    model: Option<usize>,
    effort: usize,
    effort_values: Vec<ReasoningEffort>,
    /// Effective profile when the selector opened, used to preserve the
    /// current effort for the current model and to prefer each model's own
    /// default when the model changes.
    current_provider: String,
    current_model: String,
    current_effort: ReasoningEffort,
}

impl ProfileSelector {
    #[must_use]
    pub fn new(
        catalog: InferenceCatalog,
        current_provider: &str,
        current_model: &str,
        current_effort: ReasoningEffort,
    ) -> Self {
        let provider = catalog.provider_index(current_provider);
        let model = catalog
            .providers
            .get(provider)
            .and_then(|p| p.models.iter().position(|m| m.id == current_model));
        let effort_values = Self::efforts_for(&catalog, provider, model);
        let effort = effort_values
            .iter()
            .position(|e| *e == current_effort)
            .unwrap_or(0);
        Self {
            catalog,
            phase: ProfilePhase::Provider,
            selected: provider,
            provider,
            model,
            effort,
            effort_values,
            current_provider: current_provider.to_owned(),
            current_model: current_model.to_owned(),
            current_effort,
        }
    }

    fn efforts_for(
        catalog: &InferenceCatalog,
        provider: usize,
        model: Option<usize>,
    ) -> Vec<ReasoningEffort> {
        model
            .and_then(|model| catalog.providers.get(provider)?.models.get(model))
            .map(CatalogModel::selectable_efforts)
            .unwrap_or_else(|| vec![ReasoningEffort::ProviderDefault])
    }

    /// The model a provider switch preselects: its configured default, or
    /// `None` when the default is not in the live catalog. The selector must
    /// never silently choose a different model.
    fn default_model_index(catalog: &InferenceCatalog, provider: usize) -> Option<usize> {
        let provider = catalog.providers.get(provider)?;
        provider
            .models
            .iter()
            .position(|model| model.id == provider.default_model)
    }

    #[must_use]
    pub fn title(&self) -> String {
        match self.phase {
            ProfilePhase::Provider => "Inference profile · provider".to_owned(),
            ProfilePhase::Model => format!(
                "Inference profile · {}",
                self.current_provider()
                    .map(|p| p.display_name.as_str())
                    .unwrap_or("provider")
            ),
            ProfilePhase::Effort => format!(
                "Inference profile · {}",
                self.current_model()
                    .map(CatalogModel::label)
                    .unwrap_or_else(|| "model".to_owned())
            ),
        }
    }

    #[must_use]
    pub fn hint(&self) -> &'static str {
        "↑↓ select · enter choose · backspace back · esc cancel"
    }

    fn current_provider(&self) -> Option<&CatalogProvider> {
        self.catalog.providers.get(self.provider)
    }

    fn current_model(&self) -> Option<&CatalogModel> {
        self.model
            .and_then(|model| self.current_provider()?.models.get(model))
    }

    #[must_use]
    pub fn rows(&self) -> Vec<ChoiceRow> {
        let row = |index: usize, label: String, description: String, current: bool| ChoiceRow {
            label,
            description,
            current,
            selected: index == self.selected,
        };
        match self.phase {
            ProfilePhase::Provider => self
                .catalog
                .providers
                .iter()
                .enumerate()
                .map(|(index, provider)| {
                    row(
                        index,
                        provider.display_name.clone(),
                        provider.id.clone(),
                        index == self.provider,
                    )
                })
                .collect(),
            ProfilePhase::Model => {
                let Some(provider) = self.current_provider() else {
                    return Vec::new();
                };
                let mut rows: Vec<ChoiceRow> = provider
                    .models
                    .iter()
                    .enumerate()
                    .map(|(index, model)| {
                        row(
                            index,
                            model.label(),
                            model.id.clone(),
                            self.model == Some(index),
                        )
                    })
                    .collect();
                rows.push(row(
                    provider.models.len(),
                    "Change provider…".to_owned(),
                    String::new(),
                    false,
                ));
                rows
            }
            ProfilePhase::Effort => self
                .effort_values
                .iter()
                .enumerate()
                .map(|(index, effort)| {
                    row(
                        index,
                        effort.label().to_owned(),
                        String::new(),
                        index == self.effort,
                    )
                })
                .collect(),
        }
    }

    #[must_use]
    fn row_count(&self) -> usize {
        match self.phase {
            ProfilePhase::Provider => self.catalog.providers.len(),
            ProfilePhase::Model => self.current_provider().map_or(0, |p| p.models.len() + 1),
            ProfilePhase::Effort => self.effort_values.len(),
        }
    }

    pub fn up(&mut self) {
        let len = self.row_count();
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    pub fn down(&mut self) {
        let len = self.row_count();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    /// Moves to the previous phase. Returns false when already at the first
    /// phase, where the caller cancels the whole selector instead.
    pub fn back(&mut self) -> bool {
        match self.phase {
            ProfilePhase::Provider => false,
            ProfilePhase::Model => {
                self.phase = ProfilePhase::Provider;
                self.selected = self.provider;
                true
            }
            ProfilePhase::Effort => {
                self.phase = ProfilePhase::Model;
                self.selected = self.model.unwrap_or(0);
                true
            }
        }
    }

    /// Confirms the highlighted row. `Some((provider, model, effort))` is
    /// returned only when the whole profile is chosen.
    pub fn confirm(&mut self) -> Option<(String, String, ReasoningEffort)> {
        match self.phase {
            ProfilePhase::Provider => {
                if self.catalog.providers.is_empty() {
                    return None;
                }
                self.provider = self.selected.min(self.catalog.providers.len() - 1);
                // Staying on the current provider keeps its current model;
                // switching providers lands on that provider's configured
                // default model. Index 0 is never chosen implicitly.
                let same_provider = self
                    .catalog
                    .providers
                    .get(self.provider)
                    .is_some_and(|provider| provider.id == self.current_provider);
                self.model = if same_provider {
                    self.current_provider().and_then(|provider| {
                        provider
                            .models
                            .iter()
                            .position(|model| model.id == self.current_model)
                    })
                } else {
                    Self::default_model_index(&self.catalog, self.provider)
                };
                self.effort_values = Self::efforts_for(&self.catalog, self.provider, self.model);
                self.effort = 0;
                self.phase = ProfilePhase::Model;
                self.selected = self.model.unwrap_or(0);
                None
            }
            ProfilePhase::Model => {
                let (provider_id, model_id) = {
                    let provider = self.current_provider()?;
                    if self.selected >= provider.models.len() {
                        // "Change provider…"
                        self.phase = ProfilePhase::Provider;
                        self.selected = self.provider;
                        return None;
                    }
                    (
                        provider.id.clone(),
                        provider.models.get(self.selected).map(|m| m.id.clone()),
                    )
                };
                self.model = Some(self.selected);
                self.effort_values = Self::efforts_for(&self.catalog, self.provider, self.model);
                let same_selection = provider_id == self.current_provider
                    && model_id.as_deref() == Some(self.current_model.as_str());
                let preferred = if same_selection {
                    self.current_effort
                } else {
                    self.current_model()
                        .map(CatalogModel::preferred_effort)
                        .unwrap_or(ReasoningEffort::ProviderDefault)
                };
                self.effort = self
                    .effort_values
                    .iter()
                    .position(|effort| *effort == preferred)
                    .unwrap_or(0);
                self.phase = ProfilePhase::Effort;
                self.selected = self.effort;
                None
            }
            ProfilePhase::Effort => {
                let provider = self.current_provider()?.id.clone();
                let model = self.current_model()?.id.clone();
                self.effort = self
                    .selected
                    .min(self.effort_values.len().saturating_sub(1));
                let effort = self.effort_values.get(self.effort).copied()?;
                Some((provider, model, effort))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> InferenceCatalog {
        InferenceCatalog {
            providers: vec![
                CatalogProvider {
                    id: "opencode-go".into(),
                    display_name: "OpenCode Go".into(),
                    default_model: String::new(),
                    models: vec![CatalogModel {
                        id: "deepseek-v4.1-flash".into(),
                        display_name: "DeepSeek V4.1 Flash".into(),
                        efforts: vec![
                            ReasoningEffort::Low,
                            ReasoningEffort::High,
                            ReasoningEffort::Max,
                        ],
                        default_effort: ReasoningEffort::Low,
                        input_modalities: vec![InputModality::Text],
                    }],
                },
                CatalogProvider {
                    id: "anthropic".into(),
                    display_name: "Anthropic".into(),
                    default_model: String::new(),
                    models: vec![CatalogModel {
                        id: "claude-sonnet-4-5".into(),
                        display_name: "Claude Sonnet 4.5".into(),
                        efforts: vec![],
                        default_effort: ReasoningEffort::ProviderDefault,
                        input_modalities: vec![InputModality::Text],
                    }],
                },
            ],
        }
    }

    #[test]
    fn profile_selector_walks_provider_model_effort() {
        let mut selector = ProfileSelector::new(
            catalog(),
            "opencode-go",
            "deepseek-v4.1-flash",
            ReasoningEffort::Low,
        );
        assert!(selector.title().contains("provider"));
        // Choose the highlighted provider (OpenCode Go).
        assert!(selector.confirm().is_none());
        assert!(selector.title().contains("OpenCode Go"));
        assert!(selector.confirm().is_none());
        assert!(selector.title().contains("DeepSeek"));
        // Effort rows only include supported values plus provider default, and
        // re-selecting the current model preserves the current effort.
        let rows = selector.rows();
        assert_eq!(
            rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(),
            vec!["provider default", "low", "high", "max"]
        );
        assert!(
            rows.iter().any(|row| row.selected && row.label == "low"),
            "current effort stays highlighted"
        );
        let (provider, model, effort) = selector.confirm().expect("profile");
        assert_eq!(provider, "opencode-go");
        assert_eq!(model, "deepseek-v4.1-flash");
        assert_eq!(effort, ReasoningEffort::Low);
    }

    #[test]
    fn changing_model_highlights_that_models_default_effort() {
        let mut catalog = catalog();
        catalog.providers[0].models.push(CatalogModel {
            id: "other-model".into(),
            display_name: "Other".into(),
            efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max,
            ],
            default_effort: ReasoningEffort::High,
            input_modalities: vec![InputModality::Text],
        });
        let mut selector = ProfileSelector::new(
            catalog,
            "opencode-go",
            "deepseek-v4.1-flash",
            ReasoningEffort::Low,
        );
        selector.confirm(); // provider
        selector.down(); // select other-model
        selector.confirm(); // model -> effort phase
        let rows = selector.rows();
        assert!(
            rows.iter().any(|row| row.selected && row.label == "high"),
            "a newly selected model highlights its own default effort: {rows:?}"
        );
    }

    #[test]
    fn profile_selector_offers_provider_change_from_model_step() {
        let mut selector = ProfileSelector::new(
            catalog(),
            "opencode-go",
            "deepseek-v4.1-flash",
            ReasoningEffort::ProviderDefault,
        );
        selector.confirm(); // provider
        // Move to the trailing "Change provider…" row.
        selector.down();
        assert!(selector.confirm().is_none());
        assert!(selector.title().contains("provider"));
    }

    #[test]
    fn provider_switch_preselects_the_configured_default_model_not_index_zero() {
        let mut catalog = catalog();
        catalog.providers[0].models.insert(
            0,
            CatalogModel {
                id: "first-in-catalog".into(),
                display_name: "First".into(),
                efforts: vec![],
                default_effort: ReasoningEffort::ProviderDefault,
                input_modalities: vec![InputModality::Text],
            },
        );
        catalog.providers[0].default_model = "deepseek-v4.1-flash".into();
        let mut selector = ProfileSelector::new(
            catalog,
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningEffort::ProviderDefault,
        );
        selector.up(); // move to OpenCode Go
        selector.confirm(); // provider -> model step
        let rows = selector.rows();
        assert!(
            rows.iter()
                .any(|row| row.selected && row.label == "DeepSeek V4.1 Flash"),
            "the configured default is highlighted: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.selected && row.label == "First"),
            "catalog index 0 is never selected implicitly: {rows:?}"
        );
        let (provider, model, _effort) = {
            assert!(selector.confirm().is_none(), "model -> effort");
            selector.confirm().expect("profile")
        };
        assert_eq!(provider, "opencode-go");
        assert_eq!(model, "deepseek-v4.1-flash");
    }

    #[test]
    fn provider_without_a_resolved_default_preselects_no_model() {
        // Opening on another provider and switching to one whose default is
        // not in the live catalog leaves nothing preselected.
        let mut selector = ProfileSelector::new(
            catalog(),
            "opencode-go",
            "deepseek-v4.1-flash",
            ReasoningEffort::ProviderDefault,
        );
        selector.up(); // Anthropic, whose fixture default_model is empty
        selector.confirm();
        assert!(
            selector.rows().iter().all(|row| !row.current),
            "nothing is current without a configured default"
        );
    }

    #[test]
    fn back_tracks_steps_and_never_changes_the_profile_by_itself() {
        let mut selector = ProfileSelector::new(
            catalog(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningEffort::ProviderDefault,
        );
        assert!(selector.confirm().is_none()); // model step
        assert!(selector.back()); // provider step
        assert!(!selector.back()); // at the first step
        // The model with no effort support offers only provider default.
        let mut selector = ProfileSelector::new(
            catalog(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningEffort::Max,
        );
        selector.confirm();
        selector.confirm();
        let rows = selector.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "provider default");
    }

    #[test]
    fn debug_output_never_formats_secret_material() {
        let credential = SetupCredential::Secret("sk-super-secret".into());
        assert!(!format!("{credential:?}").contains("sk-super-secret"));
        let plan = SetupPlan::Apply {
            name: "openai".into(),
            provider_kind: "openai".into(),
            base_url: None,
            credential: credential.clone(),
            model: "gpt-5.5".into(),
            enabled_models: None,
            custom_model_display_name: None,
            custom_transport: None,
            effort: ReasoningEffort::ProviderDefault,
        };
        assert!(
            !format!("{plan:?}").contains("sk-super-secret"),
            "a setup plan debug log must not leak the typed secret"
        );
    }
}
