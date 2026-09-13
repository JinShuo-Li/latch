//! Provider/catalog-facing selector state machines for `/model` and `/setup`.
//!
//! The TUI never inspects base URLs, model families, or wire parameters. The
//! CLI sends a provider-neutral catalog; these machines turn keyboard input
//! into a profile selection or a setup plan.

use latch_protocol::ReasoningEffort;

/// One selectable model as presented by the CLI catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub id: String,
    pub display_name: String,
    pub efforts: Vec<ReasoningEffort>,
    pub default_effort: ReasoningEffort,
}

impl CatalogModel {
    /// Selectable efforts, always including `provider default`.
    #[must_use]
    pub fn selectable_efforts(&self) -> Vec<ReasoningEffort> {
        let mut efforts = vec![ReasoningEffort::ProviderDefault];
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            if self.efforts.contains(&effort) {
                efforts.push(effort);
            }
        }
        efforts
    }

    #[must_use]
    pub fn label(&self) -> &str {
        if self.display_name.trim().is_empty() {
            &self.id
        } else {
            &self.display_name
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogProvider {
    pub id: String,
    pub display_name: String,
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
    pub credential_label: String,
    pub default_model: String,
    pub models: Vec<CatalogModel>,
}

/// A setup flow result that must leave the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupPlan {
    Apply {
        provider_kind: String,
        base_url: Option<String>,
        credential: SetupCredential,
        model: String,
        effort: ReasoningEffort,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupCredential {
    /// Reference an environment variable by name.
    Env(String),
    /// Enter a secret now; the CLI stores it in the 0600 local secrets file.
    Secret(String),
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
                    .unwrap_or("model")
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
                        row(
                            index,
                            model.label().to_owned(),
                            model.id.clone(),
                            index == self.model,
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
                let provider = self.current_provider()?;
                if self.selected >= provider.models.len() {
                    // "Change provider…"
                    self.phase = ProfilePhase::Provider;
                    self.selected = self.provider;
                    return None;
                }
                self.model = self.selected;
                self.effort_values = Self::efforts_for(&self.catalog, self.provider, self.model);
                self.effort = 0;
                self.phase = ProfilePhase::Effort;
                self.selected = 0;
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
    Kind,
    Endpoint,
    Credential,
    EnvName,
    Secret,
    Model,
    Effort,
    Review,
}

/// `/setup`: guided persistent provider configuration.
#[derive(Debug, Clone)]
pub struct SetupFlow {
    kinds: Vec<SetupKind>,
    step: SetupStep,
    selected: usize,
    kind: usize,
    endpoint: String,
    /// True: environment variable; false: enter a secret now.
    use_env: bool,
    env_name: String,
    secret: String,
    model: usize,
    effort: usize,
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
            endpoint: String::new(),
            use_env: true,
            env_name: String::new(),
            secret: String::new(),
            model: 0,
            effort: 0,
        };
        flow.reset_endpoint();
        flow
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
            SetupStep::Kind => "Setup · provider".to_owned(),
            SetupStep::Endpoint => "Setup · endpoint".to_owned(),
            SetupStep::Credential => "Setup · credential source".to_owned(),
            SetupStep::EnvName => "Setup · environment variable".to_owned(),
            SetupStep::Secret => "Setup · API key".to_owned(),
            SetupStep::Model => "Setup · model".to_owned(),
            SetupStep::Effort => "Setup · reasoning effort".to_owned(),
            SetupStep::Review => "Setup · review".to_owned(),
        }
    }

    #[must_use]
    pub fn hint(&self) -> &'static str {
        match self.step {
            SetupStep::Kind | SetupStep::Credential | SetupStep::Model | SetupStep::Effort => {
                "↑↓ select · enter next · esc cancel"
            }
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
            SetupStep::Model => self
                .current_kind()
                .map(|kind| kind.models.clone())
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(index, model)| {
                    row(
                        index,
                        model.label().to_owned(),
                        model.id.clone(),
                        index == self.model,
                    )
                })
                .collect(),
            SetupStep::Effort => self
                .current_model()
                .map(CatalogModel::selectable_efforts)
                .unwrap_or_default()
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
            SetupStep::Endpoint | SetupStep::EnvName | SetupStep::Secret => Vec::new(),
        }
    }

    /// The review summary shown on the review step. Never contains the secret.
    #[must_use]
    pub fn review_lines(&self) -> Vec<(String, String)> {
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
            ("Provider".to_owned(), kind),
            ("Endpoint".to_owned(), self.endpoint.clone()),
            (
                "Model".to_owned(),
                self.current_model()
                    .map(|m| m.id.clone())
                    .unwrap_or_default(),
            ),
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

    fn selected_effort(&self) -> ReasoningEffort {
        self.current_model()
            .map(CatalogModel::selectable_efforts)
            .and_then(|efforts| efforts.get(self.effort).copied())
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
            _ => {}
        }
        self.selected = 0;
    }

    pub fn back(&mut self) -> bool {
        match self.step {
            SetupStep::Kind => false,
            SetupStep::Endpoint => {
                self.step = SetupStep::Kind;
                self.selected = self.kind;
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
            SetupStep::Kind => {
                if self.kinds.is_empty() {
                    return SetupStepOutcome::Cancel;
                }
                self.kind = self.selected.min(self.kinds.len() - 1);
                self.reset_endpoint();
                self.step = SetupStep::Endpoint;
                self.selected = 0;
                SetupStepOutcome::Capture(CaptureSpec {
                    label: "endpoint".to_owned(),
                    initial: self.endpoint.clone(),
                    masked: false,
                })
            }
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
                if count == 0 {
                    return SetupStepOutcome::Cancel;
                }
                self.model = self.selected.min(count - 1);
                self.effort = 0;
                self.step = SetupStep::Effort;
                self.selected = 0;
                SetupStepOutcome::None
            }
            SetupStep::Effort => {
                let efforts = self
                    .current_model()
                    .map(CatalogModel::selectable_efforts)
                    .unwrap_or_default();
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
                    provider_kind: self
                        .current_kind()
                        .map(|kind| kind.kind.clone())
                        .unwrap_or_default(),
                    base_url: Some(self.endpoint.clone()).filter(|url| !url.trim().is_empty()),
                    credential,
                    model: self
                        .current_model()
                        .map(|model| model.id.clone())
                        .unwrap_or_default(),
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
                    models: vec![CatalogModel {
                        id: "deepseek-v4.1-flash".into(),
                        display_name: "DeepSeek V4.1 Flash".into(),
                        efforts: vec![
                            ReasoningEffort::Low,
                            ReasoningEffort::High,
                            ReasoningEffort::Max,
                        ],
                        default_effort: ReasoningEffort::Low,
                    }],
                },
                CatalogProvider {
                    id: "anthropic".into(),
                    display_name: "Anthropic".into(),
                    models: vec![CatalogModel {
                        id: "claude-sonnet-4-5".into(),
                        display_name: "Claude Sonnet 4.5".into(),
                        efforts: vec![],
                        default_effort: ReasoningEffort::ProviderDefault,
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
        // Effort rows only include supported values plus provider default.
        let rows = selector.rows();
        assert_eq!(
            rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(),
            vec!["provider default", "low", "high", "max"]
        );
        selector.down(); // low
        let (provider, model, effort) = selector.confirm().expect("profile");
        assert_eq!(provider, "opencode-go");
        assert_eq!(model, "deepseek-v4.1-flash");
        assert_eq!(effort, ReasoningEffort::Low);
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
            }],
        }];
        let mut flow = SetupFlow::new(kinds);
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
            provider_kind,
            credential,
            model,
            effort,
            ..
        }) = outcome
        else {
            panic!("expected apply, got {outcome:?}");
        };
        assert_eq!(provider_kind, "deepseek");
        assert_eq!(model, "deepseek-v4.1-flash");
        assert_eq!(effort, ReasoningEffort::High);
        assert_eq!(credential, SetupCredential::Env("DEEPSEEK_API_KEY".into()));
    }

    #[test]
    fn setup_secret_entry_never_echoes_the_value() {
        let kinds = vec![SetupKind {
            kind: "openai".into(),
            label: "OpenAI".into(),
            default_base_url: "https://api.openai.com/v1".into(),
            credential_label: "env:OPENAI_API_KEY".into(),
            default_model: "gpt-5.5".into(),
            models: vec![CatalogModel {
                id: "gpt-5.5".into(),
                display_name: "GPT-5.5".into(),
                efforts: vec![ReasoningEffort::High],
                default_effort: ReasoningEffort::ProviderDefault,
            }],
        }];
        let mut flow = SetupFlow::new(kinds);
        flow.confirm(); // kind -> endpoint
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
