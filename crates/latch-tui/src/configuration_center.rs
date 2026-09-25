//! Provider-list navigation state for `/setup`.
//!
//! This layer contains display state and actions only. Credential lookup,
//! model discovery, and persistence belong to the CLI/kernel.

use crate::profile::{ChoiceRow, SetupCredential, SetupKind};
use std::collections::BTreeSet;
mod known;
pub use known::{KnownPhase, KnownProviderFlow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSummary {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    pub status: ProviderStatus,
    pub model_count: usize,
    pub default_model: String,
    pub credential_ref: String,
    /// Enabled models with resolved transport, eligible as provider defaults.
    pub available_models: Vec<String>,
    pub models: Vec<ProviderModelSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModelSummary {
    pub id: String,
    pub display_name: String,
    pub enabled: bool,
    pub resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CenterPage {
    Providers,
    AddKind,
    Provider(String),
    Credential(String),
    Models(String),
    DefaultModel(String),
    RemoveConfirm(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CenterAction {
    StartAdd(String),
    EditCredential { name: String, secret: bool },
    SetEnabledModels { name: String, models: Vec<String> },
    SetProviderDefault { name: String, model: String },
    EditAdvanced(String),
    SetNewSessionDefault(String),
    Remove(String),
}

#[derive(Debug, Clone)]
pub struct ConfigurationCenter {
    providers: Vec<ProviderSummary>,
    kinds: Vec<SetupKind>,
    page: CenterPage,
    selected: usize,
    draft_models: BTreeSet<String>,
}

impl ConfigurationCenter {
    pub fn new(providers: Vec<ProviderSummary>, kinds: Vec<SetupKind>) -> Self {
        Self {
            providers,
            kinds,
            page: CenterPage::Providers,
            selected: 0,
            draft_models: BTreeSet::new(),
        }
    }

    pub fn page(&self) -> &CenterPage {
        &self.page
    }

    pub fn title(&self) -> String {
        match &self.page {
            CenterPage::Providers => "Setup · providers".to_owned(),
            CenterPage::AddKind => "Setup · add provider".to_owned(),
            CenterPage::Provider(id) => format!("Setup · {id}"),
            CenterPage::Credential(id) => format!("Setup · {id} · Credential"),
            CenterPage::Models(id) => format!("Setup · {id} · Models"),
            CenterPage::DefaultModel(id) => format!("Setup · {id} · Default model"),
            CenterPage::RemoveConfirm(id) => format!("Setup · remove {id}"),
        }
    }

    pub fn rows(&self) -> Vec<ChoiceRow> {
        let labels: Vec<(String, String)> = match &self.page {
            CenterPage::Providers => std::iter::once((
                "Add provider…".to_owned(),
                "OpenCode Go · OpenCode Zen · OpenAI · Anthropic · DeepSeek · Custom".to_owned(),
            ))
            .chain(self.providers.iter().map(|provider| {
                (
                    format!("{} ({})", provider.display_name, provider.id),
                    format!(
                        "{} · {} models · default {}",
                        provider.status.label(),
                        provider.model_count,
                        provider.default_model
                    ),
                )
            }))
            .collect(),
            CenterPage::AddKind => self
                .kinds
                .iter()
                .map(|kind| (kind.label.clone(), kind.kind.clone()))
                .collect(),
            CenterPage::Provider(id) => {
                let credential = self
                    .providers
                    .iter()
                    .find(|provider| &provider.id == id)
                    .map(|provider| provider.credential_ref.as_str())
                    .unwrap_or("");
                vec![
                    ("Credential".to_owned(), credential.to_owned()),
                    ("Models".to_owned(), "select and add models".to_owned()),
                    (
                        "Default model".to_owned(),
                        "provider switch default".to_owned(),
                    ),
                    (
                        "Advanced".to_owned(),
                        "transport and capabilities".to_owned(),
                    ),
                    ("Set as new-session default".to_owned(), String::new()),
                    (
                        "Remove provider".to_owned(),
                        "config only; stored credentials kept".to_owned(),
                    ),
                ]
            }
            CenterPage::Credential(id) => {
                let reference = self
                    .providers
                    .iter()
                    .find(|provider| &provider.id == id)
                    .map(|provider| provider.credential_ref.as_str())
                    .unwrap_or("");
                vec![
                    ("Environment variable".to_owned(), reference.to_owned()),
                    (
                        "Enter API key securely".to_owned(),
                        "stored 0600 locally".to_owned(),
                    ),
                ]
            }
            CenterPage::Models(id) => self
                .providers
                .iter()
                .find(|provider| &provider.id == id)
                .map(|provider| {
                    provider
                        .models
                        .iter()
                        .map(|model| {
                            let status = if !model.resolved { "unresolved" } else { "" };
                            (
                                format!(
                                    "{} {}",
                                    if self.draft_models.contains(&model.id) {
                                        "[x]"
                                    } else {
                                        "[ ]"
                                    },
                                    model.display_name
                                ),
                                format!("{} {status}", model.id),
                            )
                        })
                        .chain(std::iter::once((
                            "Save model selection".to_owned(),
                            format!("{} selected", self.draft_models.len()),
                        )))
                        .collect()
                })
                .unwrap_or_default(),
            CenterPage::DefaultModel(id) => self
                .providers
                .iter()
                .find(|provider| &provider.id == id)
                .map(|provider| {
                    provider
                        .available_models
                        .iter()
                        .map(|model| {
                            (
                                model.clone(),
                                if model == &provider.default_model {
                                    "current default".to_owned()
                                } else {
                                    String::new()
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            CenterPage::RemoveConfirm(_) => vec![
                (
                    "Remove provider".to_owned(),
                    "credentials are kept".to_owned(),
                ),
                ("Cancel".to_owned(), String::new()),
            ],
        };
        labels
            .into_iter()
            .enumerate()
            .map(|(index, (label, description))| ChoiceRow {
                label,
                description,
                current: false,
                selected: index == self.selected,
            })
            .collect()
    }

    pub fn up(&mut self) {
        let len = self.rows().len();
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    pub fn down(&mut self) {
        let len = self.rows().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    pub fn back(&mut self) -> bool {
        self.page = match &self.page {
            CenterPage::Providers => return false,
            CenterPage::AddKind | CenterPage::Provider(_) => CenterPage::Providers,
            CenterPage::DefaultModel(id) => CenterPage::Provider(id.clone()),
            CenterPage::Credential(id) => CenterPage::Provider(id.clone()),
            CenterPage::Models(id) => CenterPage::Provider(id.clone()),
            CenterPage::RemoveConfirm(id) => CenterPage::Provider(id.clone()),
        };
        self.selected = 0;
        true
    }

    pub fn confirm(&mut self) -> Option<CenterAction> {
        match &self.page {
            CenterPage::Providers => {
                self.page = if self.selected == 0 {
                    CenterPage::AddKind
                } else {
                    CenterPage::Provider(self.providers.get(self.selected - 1)?.id.clone())
                };
                self.selected = 0;
                None
            }
            CenterPage::AddKind => Some(CenterAction::StartAdd(
                self.kinds.get(self.selected)?.kind.clone(),
            )),
            CenterPage::Provider(id) => {
                let id = id.clone();
                match self.selected {
                    0 => {
                        self.page = CenterPage::Credential(id);
                        self.selected = 0;
                        None
                    }
                    1 => {
                        self.draft_models = self
                            .providers
                            .iter()
                            .find(|provider| provider.id == id)?
                            .models
                            .iter()
                            .filter(|model| model.enabled)
                            .map(|model| model.id.clone())
                            .collect();
                        self.page = CenterPage::Models(id);
                        self.selected = 0;
                        None
                    }
                    2 => {
                        self.page = CenterPage::DefaultModel(id);
                        self.selected = 0;
                        None
                    }
                    3 => Some(CenterAction::EditAdvanced(id)),
                    4 => Some(CenterAction::SetNewSessionDefault(id)),
                    _ => {
                        self.page = CenterPage::RemoveConfirm(id);
                        self.selected = 0;
                        None
                    }
                }
            }
            CenterPage::RemoveConfirm(id) => {
                if self.selected == 0 {
                    Some(CenterAction::Remove(id.clone()))
                } else {
                    self.back();
                    None
                }
            }
            CenterPage::DefaultModel(id) => Some(CenterAction::SetProviderDefault {
                name: id.clone(),
                model: self
                    .providers
                    .iter()
                    .find(|provider| &provider.id == id)?
                    .available_models
                    .get(self.selected)?
                    .clone(),
            }),
            CenterPage::Credential(id) => Some(CenterAction::EditCredential {
                name: id.clone(),
                secret: self.selected == 1,
            }),
            CenterPage::Models(id) => {
                let provider = self.providers.iter().find(|provider| &provider.id == id)?;
                if self.selected == provider.models.len() {
                    if self.draft_models.is_empty()
                        || !self.draft_models.contains(&provider.default_model)
                    {
                        return None;
                    }
                    return Some(CenterAction::SetEnabledModels {
                        name: id.clone(),
                        models: self.draft_models.iter().cloned().collect(),
                    });
                }
                let model = provider.models.get(self.selected)?;
                if model.id == provider.default_model {
                    return None;
                }
                if !self.draft_models.remove(&model.id) {
                    self.draft_models.insert(model.id.clone());
                }
                None
            }
        }
    }

    pub fn credential_from_capture(&self, value: String) -> Option<(String, SetupCredential)> {
        let CenterPage::Credential(id) = &self.page else {
            return None;
        };
        let credential = if self.selected == 1 {
            SetupCredential::Secret(value)
        } else {
            SetupCredential::Env(value.trim().to_owned())
        };
        Some((id.clone(), credential))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind() -> SetupKind {
        SetupKind {
            kind: "deepseek".into(),
            label: "DeepSeek".into(),
            default_base_url: String::new(),
            requires_base_url: false,
            credential_label: "env:DEEPSEEK_API_KEY".into(),
            default_model: "deepseek-flash".into(),
            models: vec![],
        }
    }

    #[test]
    fn first_run_highlights_add_and_list_uses_status_rows() {
        let empty = ConfigurationCenter::new(vec![], vec![kind()]);
        assert_eq!(empty.page(), &CenterPage::Providers);
        assert!(empty.rows()[0].selected);
        let provider = ProviderSummary {
            id: "deepseek".into(),
            display_name: "DeepSeek".into(),
            kind: "deepseek".into(),
            status: ProviderStatus::MissingCredential,
            model_count: 2,
            default_model: "deepseek-flash".into(),
            credential_ref: "file:deepseek".into(),
            available_models: vec!["deepseek-flash".into()],
            models: vec![],
        };
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        assert!(
            center.rows()[1]
                .description
                .contains("missing credential · 2 models")
        );
        center.down();
        center.confirm();
        assert_eq!(center.page(), &CenterPage::Provider("deepseek".into()));
        assert_eq!(center.rows()[0].description, "file:deepseek");
        center.down();
        assert_eq!(center.confirm(), None);
        assert_eq!(center.page(), &CenterPage::Models("deepseek".into()));
    }

    #[test]
    fn remove_requires_confirmation_and_back_keeps_provider() {
        let provider = ProviderSummary {
            id: "deepseek".into(),
            display_name: "DeepSeek".into(),
            kind: "deepseek".into(),
            status: ProviderStatus::Ready,
            model_count: 1,
            default_model: "deepseek-flash".into(),
            credential_ref: "env:DEEPSEEK_API_KEY".into(),
            available_models: vec!["deepseek-flash".into()],
            models: vec![],
        };
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        center.down();
        center.confirm();
        for _ in 0..5 {
            center.down();
        }
        assert_eq!(center.confirm(), None);
        assert_eq!(center.page(), &CenterPage::RemoveConfirm("deepseek".into()));
        center.down();
        assert_eq!(center.confirm(), None);
        assert_eq!(center.page(), &CenterPage::Provider("deepseek".into()));
    }

    #[test]
    fn provider_default_picker_offers_only_resolved_enabled_models() {
        let provider = ProviderSummary {
            id: "deepseek".into(),
            display_name: "DeepSeek".into(),
            kind: "deepseek".into(),
            status: ProviderStatus::Ready,
            model_count: 3,
            default_model: "deepseek-flash".into(),
            credential_ref: "env:DEEPSEEK_API_KEY".into(),
            available_models: vec!["deepseek-flash".into(), "deepseek-v4-pro".into()],
            models: vec![],
        };
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        center.down();
        center.confirm();
        center.down();
        center.down();
        center.confirm();
        assert_eq!(center.page(), &CenterPage::DefaultModel("deepseek".into()));
        assert_eq!(center.rows().len(), 2);
        center.down();
        assert_eq!(
            center.confirm(),
            Some(CenterAction::SetProviderDefault {
                name: "deepseek".into(),
                model: "deepseek-v4-pro".into()
            })
        );
    }

    #[test]
    fn credential_editor_never_echoes_a_stored_value() {
        let provider = ProviderSummary {
            id: "deepseek".into(),
            display_name: "DeepSeek".into(),
            kind: "deepseek".into(),
            status: ProviderStatus::MissingCredential,
            model_count: 1,
            default_model: "deepseek-flash".into(),
            credential_ref: "file:deepseek".into(),
            available_models: vec!["deepseek-flash".into()],
            models: vec![],
        };
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        center.down();
        center.confirm();
        center.confirm();
        assert_eq!(center.page(), &CenterPage::Credential("deepseek".into()));
        assert_eq!(
            center.confirm(),
            Some(CenterAction::EditCredential {
                name: "deepseek".into(),
                secret: false,
            })
        );
        center.down();
        assert_eq!(
            center.confirm(),
            Some(CenterAction::EditCredential {
                name: "deepseek".into(),
                secret: true,
            })
        );
        assert_eq!(
            center.credential_from_capture("private-value".into()),
            Some((
                "deepseek".into(),
                SetupCredential::Secret("private-value".into())
            ))
        );
        assert!(!format!("{center:?}").contains("private-value"));
    }

    #[test]
    fn model_editor_keeps_default_enabled_and_shows_unresolved_entries() {
        let provider = ProviderSummary {
            id: "custom".into(),
            display_name: "Custom".into(),
            kind: "custom".into(),
            status: ProviderStatus::UnresolvedModels,
            model_count: 1,
            default_model: "ready".into(),
            credential_ref: "env:CUSTOM_KEY".into(),
            available_models: vec!["ready".into()],
            models: vec![
                ProviderModelSummary {
                    id: "ready".into(),
                    display_name: "Ready".into(),
                    enabled: true,
                    resolved: true,
                },
                ProviderModelSummary {
                    id: "unknown".into(),
                    display_name: "Unknown".into(),
                    enabled: false,
                    resolved: false,
                },
            ],
        };
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        center.down();
        center.confirm();
        center.down();
        center.confirm();
        assert_eq!(center.page(), &CenterPage::Models("custom".into()));
        assert!(center.rows()[1].description.contains("unresolved"));
        center.confirm(); // Default cannot be deselected.
        assert!(center.rows()[0].label.starts_with("[x]"));
        center.down();
        center.confirm(); // Enable unresolved in setup only.
        center.down();
        assert_eq!(
            center.confirm(),
            Some(CenterAction::SetEnabledModels {
                name: "custom".into(),
                models: vec!["ready".into(), "unknown".into()],
            })
        );
    }

    #[test]
    fn explicit_new_session_default_emits_a_distinct_action() {
        let provider = ProviderSummary {
            id: "deepseek".into(),
            display_name: "DeepSeek".into(),
            kind: "deepseek".into(),
            status: ProviderStatus::Ready,
            model_count: 1,
            default_model: "deepseek-flash".into(),
            credential_ref: "env:DEEPSEEK_API_KEY".into(),
            available_models: vec!["deepseek-flash".into()],
            models: vec![],
        };
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        center.down();
        center.confirm();
        for _ in 0..4 {
            center.down();
        }
        assert_eq!(
            center.confirm(),
            Some(CenterAction::SetNewSessionDefault("deepseek".into()))
        );
    }
}
