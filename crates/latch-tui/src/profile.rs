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
    fn model_index(&self, provider: &CatalogProvider, id: &str) -> usize {
        provider.models.iter().position(|m| m.id == id).unwrap_or(0)
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

/// A setup flow result that must leave the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        effort: ReasoningEffort,
    },
    /// Remove one configured provider instance. Credentials are never deleted.
    Remove { name: String },
}

/// One configured provider instance shown by the removal surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredProvider {
    pub id: String,
    pub model: String,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    model: usize,
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
            .map(|p| catalog.model_index(p, current_model))
            .unwrap_or(0);
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
        model: usize,
    ) -> Vec<ReasoningEffort> {
        catalog
            .providers
            .get(provider)
            .and_then(|p| p.models.get(model))
            .map(CatalogModel::selectable_efforts)
            .unwrap_or_else(|| vec![ReasoningEffort::ProviderDefault])
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
        self.current_provider()?.models.get(self.model)
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
                        row(index, model.label(), model.id.clone(), index == self.model)
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
                self.selected = self.model;
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
                self.model = 0;
                self.effort_values = Self::efforts_for(&self.catalog, self.provider, 0);
                self.effort = 0;
                self.phase = ProfilePhase::Model;
                self.selected = 0;
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
                self.model = self.selected;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    Action,
    RemoveSelect,
    RemoveConfirm,
    Kind,
    Name,
    Endpoint,
    Credential,
    EnvName,
    Secret,
    Model,
    ModelId,
    Effort,
    Review,
}

/// `/setup`: guided persistent provider configuration.
#[derive(Clone)]
pub struct SetupFlow {
    kinds: Vec<SetupKind>,
    step: SetupStep,
    selected: usize,
    kind: usize,
    /// Stable provider instance id. Defaulted from the kind but editable so
    /// multiple instances of one kind are possible.
    name: String,
    endpoint: String,
    /// True: environment variable; false: enter a secret now.
    use_env: bool,
    env_name: String,
    secret: String,
    model: usize,
    /// Model id typed by the user when it is not in the catalog.
    custom_model: Option<String>,
    effort: usize,
    configured: Vec<ConfiguredProvider>,
    remove: usize,
}

impl std::fmt::Debug for SetupFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupFlow")
            .field("step", &self.step)
            .field("kind", &self.kind)
            .field("use_env", &self.use_env)
            // `secret` is intentionally omitted.
            .finish_non_exhaustive()
    }
}

impl SetupFlow {
    #[must_use]
    pub fn new(kinds: Vec<SetupKind>) -> Self {
        let kind = 0;
        let mut flow = Self {
            kinds,
            step: SetupStep::Kind,
            selected: 0,
            kind,
            name: String::new(),
            endpoint: String::new(),
            use_env: true,
            env_name: String::new(),
            secret: String::new(),
            model: 0,
            custom_model: None,
            effort: 0,
            configured: Vec::new(),
            remove: 0,
        };
        flow.reset_endpoint();
        flow
    }

    /// Adds the configured provider instances and starts on the add/remove
    /// menu. An empty list keeps the flow on the original add/edit path.
    #[must_use]
    pub fn with_providers(mut self, configured: Vec<ConfiguredProvider>) -> Self {
        if !configured.is_empty() {
            self.configured = configured;
            self.step = SetupStep::Action;
            self.selected = 0;
        }
        self
    }

    fn removing(&self) -> Option<&ConfiguredProvider> {
        self.configured.get(self.remove)
    }

    fn current_kind(&self) -> Option<&SetupKind> {
        self.kinds.get(self.kind)
    }

    fn reset_endpoint(&mut self) {
        self.endpoint = self
            .current_kind()
            .map(|kind| kind.default_base_url.clone())
            .unwrap_or_default();
    }

    #[must_use]
    pub fn step(&self) -> SetupStep {
        self.step
    }

    #[must_use]
    pub fn title(&self) -> String {
        match self.step {
            SetupStep::Action => "Setup · providers".to_owned(),
            SetupStep::RemoveSelect => "Setup · remove provider".to_owned(),
            SetupStep::RemoveConfirm => "Setup · confirm removal".to_owned(),
            SetupStep::Kind => "Setup · provider".to_owned(),
            SetupStep::Name => "Setup · provider name".to_owned(),
            SetupStep::Endpoint => "Setup · endpoint".to_owned(),
            SetupStep::Credential => "Setup · credential source".to_owned(),
            SetupStep::EnvName => "Setup · environment variable".to_owned(),
            SetupStep::Secret => "Setup · API key".to_owned(),
            SetupStep::Model => "Setup · model".to_owned(),
            SetupStep::ModelId => "Setup · model id".to_owned(),
            SetupStep::Effort => "Setup · reasoning effort".to_owned(),
            SetupStep::Review => "Setup · review".to_owned(),
        }
    }

    #[must_use]
    pub fn hint(&self) -> &'static str {
        match self.step {
            SetupStep::Action
            | SetupStep::RemoveSelect
            | SetupStep::Kind
            | SetupStep::Credential
            | SetupStep::Model
            | SetupStep::Effort => "↑↓ select · enter next · esc cancel",
            SetupStep::RemoveConfirm => "↑↓ select · enter confirm · esc cancel",
            SetupStep::Review => "↑↓ select · enter apply · esc cancel",
            _ => "enter next · esc cancel",
        }
    }

    #[must_use]
    pub fn rows(&self) -> Vec<ChoiceRow> {
        let row = |index: usize, label: String, description: String, current: bool| ChoiceRow {
            label,
            description,
            current,
            selected: index == self.selected,
        };
        match self.step {
            SetupStep::Action => {
                let mut rows = vec![row(
                    0,
                    "Add or edit provider…".to_owned(),
                    "configure endpoint, credential, model, and effort".to_owned(),
                    false,
                )];
                rows.push(row(
                    1,
                    "Remove provider…".to_owned(),
                    format!("{} configured", self.configured.len()),
                    false,
                ));
                rows
            }
            SetupStep::RemoveSelect => self
                .configured
                .iter()
                .enumerate()
                .map(|(index, provider)| {
                    row(
                        index,
                        provider.id.clone(),
                        provider.model.clone(),
                        index == self.remove,
                    )
                })
                .collect(),
            SetupStep::RemoveConfirm => vec![
                row(
                    0,
                    "Remove".to_owned(),
                    "config only; credentials kept".to_owned(),
                    self.selected == 0,
                ),
                row(1, "Cancel".to_owned(), String::new(), self.selected == 1),
            ],
            SetupStep::Kind => self
                .kinds
                .iter()
                .enumerate()
                .map(|(index, kind)| {
                    row(
                        index,
                        kind.label.clone(),
                        kind.kind.clone(),
                        index == self.kind,
                    )
                })
                .collect(),
            SetupStep::Credential => vec![
                row(
                    0,
                    "Use environment variable".to_owned(),
                    self.current_kind()
                        .map(|kind| kind.credential_label.clone())
                        .unwrap_or_default(),
                    self.use_env,
                ),
                row(
                    1,
                    "Enter API key securely".to_owned(),
                    "stored 0600 locally".to_owned(),
                    !self.use_env,
                ),
            ],
            SetupStep::Model => {
                let models = self
                    .current_kind()
                    .map(|kind| kind.models.clone())
                    .unwrap_or_default();
                let mut rows: Vec<ChoiceRow> = models
                    .iter()
                    .enumerate()
                    .map(|(index, model)| {
                        row(index, model.label(), model.id.clone(), index == self.model)
                    })
                    .collect();
                rows.push(row(
                    models.len(),
                    "Enter custom model…".to_owned(),
                    "conservative capabilities until metadata is added".to_owned(),
                    self.custom_model.is_some(),
                ));
                rows
            }
            SetupStep::Effort => self
                .effort_options()
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
            SetupStep::Review => vec![
                row(
                    0,
                    "Apply & save".to_owned(),
                    String::new(),
                    self.selected == 0,
                ),
                row(1, "Cancel".to_owned(), String::new(), self.selected == 1),
            ],
            SetupStep::Name
            | SetupStep::Endpoint
            | SetupStep::EnvName
            | SetupStep::Secret
            | SetupStep::ModelId => Vec::new(),
        }
    }

    /// The review summary shown on the review step. Never contains the secret.
    #[must_use]
    pub fn review_lines(&self) -> Vec<(String, String)> {
        if self.step == SetupStep::RemoveConfirm {
            let provider = self.removing().cloned().unwrap_or(ConfiguredProvider {
                id: String::new(),
                model: String::new(),
            });
            return vec![
                ("Provider".to_owned(), provider.id),
                ("Model".to_owned(), provider.model),
                (
                    "Effect".to_owned(),
                    "removes provider config only; stored credentials are kept".to_owned(),
                ),
            ];
        }
        let kind = self
            .current_kind()
            .map(|kind| kind.label.clone())
            .unwrap_or_default();
        let credential = if self.use_env {
            format!("environment variable {}", self.env_name)
        } else {
            "secure local storage (value hidden)".to_owned()
        };
        vec![
            ("Provider".to_owned(), format!("{kind} ({})", self.name)),
            ("Endpoint".to_owned(), self.endpoint.clone()),
            ("Model".to_owned(), self.effective_model_id()),
            (
                "Effort".to_owned(),
                self.selected_effort().label().to_owned(),
            ),
            ("Credential".to_owned(), credential),
        ]
    }

    fn current_model(&self) -> Option<&CatalogModel> {
        self.current_kind()?.models.get(self.model)
    }

    /// The model id this plan will persist, including a typed custom id.
    fn effective_model_id(&self) -> String {
        self.custom_model
            .clone()
            .or_else(|| self.current_model().map(|model| model.id.clone()))
            .or_else(|| self.current_kind().map(|kind| kind.default_model.clone()))
            .unwrap_or_default()
    }

    fn effort_options(&self) -> Vec<ReasoningEffort> {
        if self.custom_model.is_some() {
            return vec![ReasoningEffort::ProviderDefault];
        }
        self.current_model()
            .map(CatalogModel::selectable_efforts)
            .unwrap_or_else(|| vec![ReasoningEffort::ProviderDefault])
    }

    fn selected_effort(&self) -> ReasoningEffort {
        self.effort_options()
            .get(self.effort)
            .copied()
            .unwrap_or(ReasoningEffort::ProviderDefault)
    }

    fn row_count(&self) -> usize {
        self.rows().len()
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

    /// Applies typed text to the current capture step and advances.
    pub fn submit_capture(&mut self, value: String) {
        match self.step {
            SetupStep::Name => {
                let typed = value.trim();
                self.name = if typed.is_empty() {
                    self.current_kind()
                        .map(|kind| kind.kind.clone())
                        .unwrap_or_default()
                } else {
                    typed.to_owned()
                };
                self.step = SetupStep::Endpoint;
            }
            SetupStep::Endpoint => {
                self.endpoint = value.trim().to_owned();
                self.step = SetupStep::Credential;
            }
            SetupStep::EnvName => {
                self.env_name = value.trim().to_owned();
                self.step = SetupStep::Model;
            }
            SetupStep::Secret => {
                self.secret = value;
                self.step = SetupStep::Model;
            }
            SetupStep::ModelId => {
                let typed = value.trim();
                let matched = self
                    .current_kind()
                    .and_then(|kind| kind.models.iter().position(|model| model.id == typed));
                match matched {
                    Some(index) => {
                        self.model = index;
                        self.custom_model = None;
                    }
                    None => {
                        self.custom_model = Some(if typed.is_empty() {
                            self.current_kind()
                                .map(|kind| kind.default_model.clone())
                                .unwrap_or_default()
                        } else {
                            typed.to_owned()
                        });
                    }
                }
                self.effort = 0;
                self.step = SetupStep::Effort;
            }
            _ => {}
        }
        self.selected = 0;
    }

    pub fn back(&mut self) -> bool {
        match self.step {
            SetupStep::Action => false,
            SetupStep::RemoveSelect => {
                self.step = SetupStep::Action;
                self.selected = 1;
                true
            }
            SetupStep::RemoveConfirm => {
                self.step = SetupStep::RemoveSelect;
                self.selected = self.remove;
                true
            }
            SetupStep::Kind if !self.configured.is_empty() => {
                self.step = SetupStep::Action;
                self.selected = 0;
                true
            }
            SetupStep::Kind => false,
            SetupStep::Name => {
                self.step = SetupStep::Kind;
                self.selected = self.kind;
                true
            }
            SetupStep::Endpoint => {
                self.step = SetupStep::Name;
                true
            }
            SetupStep::Credential => {
                self.step = SetupStep::Endpoint;
                true
            }
            SetupStep::EnvName | SetupStep::Secret => {
                self.step = SetupStep::Credential;
                self.selected = usize::from(!self.use_env);
                true
            }
            SetupStep::Model => {
                self.step = if self.use_env {
                    SetupStep::EnvName
                } else {
                    SetupStep::Secret
                };
                true
            }
            SetupStep::ModelId => {
                self.step = SetupStep::Model;
                self.selected = self.model;
                true
            }
            SetupStep::Effort => {
                self.step = SetupStep::Model;
                self.selected = self.model;
                true
            }
            SetupStep::Review => {
                self.step = SetupStep::Effort;
                self.selected = self.effort;
                true
            }
        }
    }

    pub fn confirm(&mut self) -> SetupStepOutcome {
        match self.step {
            SetupStep::Action => {
                if self.selected == 1 && !self.configured.is_empty() {
                    self.step = SetupStep::RemoveSelect;
                    self.selected = 0;
                } else {
                    self.step = SetupStep::Kind;
                    self.selected = self.kind;
                }
                SetupStepOutcome::None
            }
            SetupStep::RemoveSelect => {
                if self.configured.is_empty() {
                    self.step = SetupStep::Action;
                    self.selected = 0;
                    return SetupStepOutcome::None;
                }
                self.remove = self.selected.min(self.configured.len() - 1);
                self.step = SetupStep::RemoveConfirm;
                self.selected = 0;
                SetupStepOutcome::None
            }
            SetupStep::RemoveConfirm => {
                if self.selected == 1 {
                    return SetupStepOutcome::Cancel;
                }
                SetupStepOutcome::Apply(SetupPlan::Remove {
                    name: self
                        .removing()
                        .map(|provider| provider.id.clone())
                        .unwrap_or_default(),
                })
            }
            SetupStep::Kind => {
                if self.kinds.is_empty() {
                    return SetupStepOutcome::Cancel;
                }
                self.kind = self.selected.min(self.kinds.len() - 1);
                self.name = self
                    .current_kind()
                    .map(|kind| kind.kind.clone())
                    .unwrap_or_default();
                self.custom_model = None;
                self.reset_endpoint();
                self.step = SetupStep::Name;
                self.selected = 0;
                SetupStepOutcome::Capture(CaptureSpec {
                    label: "provider name".to_owned(),
                    initial: self.name.clone(),
                    masked: false,
                })
            }
            SetupStep::Name => SetupStepOutcome::Capture(CaptureSpec {
                label: "provider name".to_owned(),
                initial: self.name.clone(),
                masked: false,
            }),
            SetupStep::Endpoint => SetupStepOutcome::Capture(CaptureSpec {
                label: "endpoint".to_owned(),
                initial: self.endpoint.clone(),
                masked: false,
            }),
            SetupStep::Credential => {
                self.use_env = self.selected == 0;
                if self.use_env {
                    if self.env_name.is_empty() {
                        self.env_name = self
                            .current_kind()
                            .map(|kind| kind.credential_label.trim_start_matches("env:").to_owned())
                            .unwrap_or_default();
                    }
                    self.step = SetupStep::EnvName;
                    SetupStepOutcome::Capture(CaptureSpec {
                        label: "environment variable".to_owned(),
                        initial: self.env_name.clone(),
                        masked: false,
                    })
                } else {
                    self.step = SetupStep::Secret;
                    SetupStepOutcome::Capture(CaptureSpec {
                        label: "API key".to_owned(),
                        initial: String::new(),
                        masked: true,
                    })
                }
            }
            SetupStep::EnvName => SetupStepOutcome::Capture(CaptureSpec {
                label: "environment variable".to_owned(),
                initial: self.env_name.clone(),
                masked: false,
            }),
            SetupStep::Secret => SetupStepOutcome::Capture(CaptureSpec {
                label: "API key".to_owned(),
                initial: String::new(),
                masked: true,
            }),
            SetupStep::Model => {
                let count = self.current_kind().map_or(0, |kind| kind.models.len());
                if self.selected >= count {
                    // "Enter custom model…" (also the only path when the
                    // provider kind ships no built-in models, e.g. custom
                    // OpenAI-compatible endpoints).
                    self.step = SetupStep::ModelId;
                    self.selected = 0;
                    return SetupStepOutcome::Capture(CaptureSpec {
                        label: "model id".to_owned(),
                        initial: self.effective_model_id(),
                        masked: false,
                    });
                }
                self.model = self.selected;
                self.custom_model = None;
                self.effort = 0;
                self.step = SetupStep::Effort;
                self.selected = 0;
                SetupStepOutcome::None
            }
            SetupStep::ModelId => SetupStepOutcome::Capture(CaptureSpec {
                label: "model id".to_owned(),
                initial: self.effective_model_id(),
                masked: false,
            }),
            SetupStep::Effort => {
                let efforts = self.effort_options();
                if efforts.is_empty() {
                    return SetupStepOutcome::Cancel;
                }
                self.effort = self.selected.min(efforts.len() - 1);
                self.step = SetupStep::Review;
                self.selected = 0;
                SetupStepOutcome::None
            }
            SetupStep::Review => {
                if self.selected == 1 {
                    return SetupStepOutcome::Cancel;
                }
                let credential = if self.use_env {
                    SetupCredential::Env(self.env_name.clone())
                } else {
                    SetupCredential::Secret(self.secret.clone())
                };
                SetupStepOutcome::Apply(SetupPlan::Apply {
                    name: self.name.clone(),
                    provider_kind: self
                        .current_kind()
                        .map(|kind| kind.kind.clone())
                        .unwrap_or_default(),
                    base_url: Some(self.endpoint.clone()).filter(|url| !url.trim().is_empty()),
                    credential,
                    model: self.effective_model_id(),
                    enabled_models: None,
                    effort: self.selected_effort(),
                })
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
    fn setup_flow_collects_endpoint_credential_model_and_effort() {
        let kinds = vec![SetupKind {
            kind: "deepseek".into(),
            label: "DeepSeek".into(),
            default_base_url: "https://api.deepseek.com".into(),
            requires_base_url: false,
            credential_label: "env:DEEPSEEK_API_KEY".into(),
            default_model: "deepseek-v4.1-flash".into(),
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
        }];
        let mut flow = SetupFlow::new(kinds);
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("deepseek".into());
        assert_eq!(flow.step(), SetupStep::Endpoint);
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("https://api.deepseek.com".into());
        assert_eq!(flow.step(), SetupStep::Credential);
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("DEEPSEEK_API_KEY".into());
        assert_eq!(flow.step(), SetupStep::Model);
        assert_eq!(flow.confirm(), SetupStepOutcome::None);
        assert_eq!(flow.step(), SetupStep::Effort);
        flow.down(); // provider default -> low
        flow.down(); // low -> high
        assert_eq!(flow.confirm(), SetupStepOutcome::None);
        assert_eq!(flow.step(), SetupStep::Review);
        let lines = flow.review_lines();
        assert!(
            lines
                .iter()
                .any(|(key, value)| key == "Credential" && value.contains("DEEPSEEK_API_KEY"))
        );
        assert!(lines.iter().all(|(_, value)| !value.contains("sk-")));
        let outcome = flow.confirm();
        let SetupStepOutcome::Apply(SetupPlan::Apply {
            name,
            provider_kind,
            credential,
            model,
            effort,
            ..
        }) = outcome
        else {
            panic!("expected apply, got {outcome:?}");
        };
        assert_eq!(name, "deepseek");
        assert_eq!(provider_kind, "deepseek");
        assert_eq!(model, "deepseek-v4.1-flash");
        assert_eq!(effort, ReasoningEffort::High);
        assert_eq!(credential, SetupCredential::Env("DEEPSEEK_API_KEY".into()));
    }

    fn setup_kind() -> SetupKind {
        SetupKind {
            kind: "deepseek".into(),
            label: "DeepSeek".into(),
            default_base_url: "https://api.deepseek.com".into(),
            requires_base_url: false,
            credential_label: "env:DEEPSEEK_API_KEY".into(),
            default_model: "deepseek-flash".into(),
            models: vec![CatalogModel {
                id: "deepseek-flash".into(),
                display_name: "DeepSeek Flash".into(),
                efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
                default_effort: ReasoningEffort::High,
                input_modalities: vec![InputModality::Text],
            }],
        }
    }

    #[test]
    fn setup_offers_removal_with_confirmation_and_cancel_preserves() {
        let configured = vec![
            ConfiguredProvider {
                id: "deepseek".into(),
                model: "deepseek-flash".into(),
            },
            ConfiguredProvider {
                id: "lab-endpoint".into(),
                model: "lab-model".into(),
            },
        ];
        // Cancel path: menu -> remove -> select -> cancel keeps the flow open
        // and applies nothing.
        let mut flow = SetupFlow::new(vec![setup_kind()]).with_providers(configured.clone());
        assert_eq!(flow.step(), SetupStep::Action);
        assert_eq!(flow.rows().len(), 2);
        flow.down(); // select "Remove provider…"
        assert_eq!(flow.confirm(), SetupStepOutcome::None);
        assert_eq!(flow.step(), SetupStep::RemoveSelect);
        flow.down(); // select the second provider
        assert_eq!(flow.confirm(), SetupStepOutcome::None);
        assert_eq!(flow.step(), SetupStep::RemoveConfirm);
        let review = flow.review_lines();
        assert!(
            review
                .iter()
                .any(|(key, value)| key == "Provider" && value == "lab-endpoint")
        );
        assert!(
            review
                .iter()
                .any(|(_, value)| value.contains("credentials are kept"))
        );
        flow.down(); // highlight Cancel
        assert_eq!(flow.confirm(), SetupStepOutcome::Cancel);

        // Confirm path: removing the first provider applies exactly that plan.
        let mut flow = SetupFlow::new(vec![setup_kind()]).with_providers(configured);
        flow.down(); // select "Remove provider…"
        flow.confirm(); // action -> remove select
        assert_eq!(flow.confirm(), SetupStepOutcome::None); // select first
        let SetupStepOutcome::Apply(SetupPlan::Remove { name }) = flow.confirm() else {
            panic!("expected removal");
        };
        assert_eq!(name, "deepseek");
    }

    #[test]
    fn setup_custom_provider_allows_manual_model_entry() {
        let kinds = vec![SetupKind {
            kind: "openai-compatible".into(),
            label: "Custom OpenAI-compatible".into(),
            default_base_url: String::new(),
            requires_base_url: true,
            credential_label: "env:OPENAI_API_KEY".into(),
            default_model: String::new(),
            models: Vec::new(),
        }];
        let mut flow = SetupFlow::new(kinds);
        flow.confirm(); // kind -> provider name
        flow.submit_capture("lab-endpoint".into());
        flow.confirm(); // endpoint
        flow.submit_capture("https://lab.example/v1".into());
        flow.confirm(); // credential
        flow.submit_capture("LAB_KEY".into());
        assert_eq!(flow.step(), SetupStep::Model);
        // No built-in models: the flow must offer a model-id capture instead
        // of cancelling.
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("lab-model-7".into());
        assert_eq!(flow.step(), SetupStep::Effort);
        assert_eq!(flow.confirm(), SetupStepOutcome::None);
        assert_eq!(flow.step(), SetupStep::Review);
        let SetupStepOutcome::Apply(SetupPlan::Apply {
            name,
            model,
            effort,
            credential,
            ..
        }) = flow.confirm()
        else {
            panic!("expected apply");
        };
        assert_eq!(name, "lab-endpoint");
        assert_eq!(model, "lab-model-7");
        assert_eq!(effort, ReasoningEffort::ProviderDefault);
        assert_eq!(credential, SetupCredential::Env("LAB_KEY".into()));
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
            effort: ReasoningEffort::ProviderDefault,
        };
        assert!(
            !format!("{plan:?}").contains("sk-super-secret"),
            "a setup plan debug log must not leak the typed secret"
        );
    }

    #[test]
    fn setup_secret_entry_never_echoes_the_value() {
        let kinds = vec![SetupKind {
            kind: "openai".into(),
            label: "OpenAI".into(),
            default_base_url: "https://api.openai.com/v1".into(),
            requires_base_url: false,
            credential_label: "env:OPENAI_API_KEY".into(),
            default_model: "gpt-5.5".into(),
            models: vec![CatalogModel {
                id: "gpt-5.5".into(),
                display_name: "GPT-5.5".into(),
                efforts: vec![ReasoningEffort::High],
                default_effort: ReasoningEffort::ProviderDefault,
                input_modalities: vec![InputModality::Text],
            }],
        }];
        let mut flow = SetupFlow::new(kinds);
        flow.confirm(); // kind -> provider name
        flow.submit_capture("openai".into());
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("https://api.openai.com/v1".into());
        flow.down(); // choose "Enter API key securely"
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("sk-live-secret".into());
        flow.confirm(); // model
        flow.confirm(); // effort
        let review = flow.review_lines();
        assert!(
            review
                .iter()
                .all(|(key, value)| !(key == "Credential" && value.contains("sk-live-secret")))
        );
        let SetupStepOutcome::Apply(SetupPlan::Apply { credential, .. }) = flow.confirm() else {
            panic!("expected apply");
        };
        assert_eq!(credential, SetupCredential::Secret("sk-live-secret".into()));
    }
}
