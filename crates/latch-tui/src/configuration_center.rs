//! Provider-list navigation state for `/setup`.
//!
//! This layer contains display state and actions only. Credential lookup,
//! model discovery, and persistence belong to the CLI/kernel.

use crate::profile::{ChoiceRow, SetupKind};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CenterPage {
    Providers,
    AddKind,
    Provider(String),
    RemoveConfirm(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CenterAction {
    StartAdd(String),
    EditCredential(String),
    EditModels(String),
    EditDefaultModel(String),
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
}

impl ConfigurationCenter {
    pub fn new(providers: Vec<ProviderSummary>, kinds: Vec<SetupKind>) -> Self {
        Self {
            providers,
            kinds,
            page: CenterPage::Providers,
            selected: 0,
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
                    0 => Some(CenterAction::EditCredential(id)),
                    1 => Some(CenterAction::EditModels(id)),
                    2 => Some(CenterAction::EditDefaultModel(id)),
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
        }
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
        assert_eq!(
            center.confirm(),
            Some(CenterAction::EditModels("deepseek".into()))
        );
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
}
