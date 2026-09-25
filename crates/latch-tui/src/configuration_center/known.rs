//! Four-choice setup for catalog-backed providers.

use crate::profile::{
    CaptureSpec, ChoiceRow, SetupCredential, SetupKind, SetupPlan, SetupStepOutcome,
};
use latch_protocol::ReasoningEffort;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownPhase {
    Credential,
    EnvName,
    Secret,
    Models,
    DefaultModel,
    Review,
}

#[derive(Clone)]
pub struct KnownProviderFlow {
    kind: SetupKind,
    phase: KnownPhase,
    selected: usize,
    use_env: bool,
    env_name: String,
    secret: String,
    enabled: BTreeSet<String>,
    default_model: String,
}

impl std::fmt::Debug for KnownProviderFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KnownProviderFlow")
            .field("kind", &self.kind.kind)
            .field("phase", &self.phase)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl KnownProviderFlow {
    pub fn new(kind: SetupKind) -> Self {
        let default_model = if kind
            .models
            .iter()
            .any(|model| model.id == kind.default_model)
        {
            kind.default_model.clone()
        } else {
            kind.models
                .first()
                .map(|model| model.id.clone())
                .unwrap_or_default()
        };
        let enabled = BTreeSet::from([default_model.clone()]);
        Self {
            env_name: kind.credential_label.trim_start_matches("env:").to_owned(),
            kind,
            phase: KnownPhase::Credential,
            selected: 0,
            use_env: true,
            secret: String::new(),
            enabled,
            default_model,
        }
    }

    pub fn phase(&self) -> KnownPhase {
        self.phase
    }

    pub fn title(&self) -> String {
        match self.phase {
            KnownPhase::Credential | KnownPhase::EnvName | KnownPhase::Secret => {
                format!("Setup · {} · Credential", self.kind.label)
            }
            KnownPhase::Models => format!("Setup · {} · Models", self.kind.label),
            KnownPhase::DefaultModel => format!("Setup · {} · Default model", self.kind.label),
            KnownPhase::Review => format!("Setup · {} · Save", self.kind.label),
        }
    }

    pub fn rows(&self) -> Vec<ChoiceRow> {
        let labels: Vec<(String, String, bool)> = match self.phase {
            KnownPhase::Credential => vec![
                (
                    "Environment variable".into(),
                    self.kind.credential_label.clone(),
                    self.use_env,
                ),
                (
                    "Enter API key securely".into(),
                    "stored 0600 locally".into(),
                    !self.use_env,
                ),
            ],
            KnownPhase::Models => self
                .kind
                .models
                .iter()
                .map(|model| {
                    (
                        model.label(),
                        model.id.clone(),
                        self.enabled.contains(&model.id),
                    )
                })
                .chain(std::iter::once((
                    "Continue to default model…".into(),
                    format!("{} selected", self.enabled.len()),
                    false,
                )))
                .collect(),
            KnownPhase::DefaultModel => self
                .kind
                .models
                .iter()
                .filter(|model| self.enabled.contains(&model.id))
                .map(|model| {
                    (
                        model.label(),
                        model.id.clone(),
                        model.id == self.default_model,
                    )
                })
                .collect(),
            KnownPhase::Review => vec![
                (
                    "Save provider".into(),
                    "config and secret staged".into(),
                    false,
                ),
                ("Cancel".into(), String::new(), false),
            ],
            KnownPhase::EnvName | KnownPhase::Secret => Vec::new(),
        };
        labels
            .into_iter()
            .enumerate()
            .map(|(index, (label, description, current))| ChoiceRow {
                label,
                description,
                current,
                selected: index == self.selected,
            })
            .collect()
    }

    pub fn review_lines(&self) -> Vec<(String, String)> {
        vec![
            (
                "Provider".into(),
                format!("{} ({})", self.kind.label, self.kind.kind),
            ),
            (
                "Credential".into(),
                if self.use_env {
                    format!("env:{}", self.env_name)
                } else {
                    "secure local storage (value hidden)".into()
                },
            ),
            (
                "Models".into(),
                self.enabled.iter().cloned().collect::<Vec<_>>().join(", "),
            ),
            ("Default".into(), self.default_model.clone()),
        ]
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
        self.phase = match self.phase {
            KnownPhase::Credential => return false,
            KnownPhase::EnvName | KnownPhase::Secret => KnownPhase::Credential,
            KnownPhase::Models => KnownPhase::Credential,
            KnownPhase::DefaultModel => KnownPhase::Models,
            KnownPhase::Review => KnownPhase::DefaultModel,
        };
        self.selected = 0;
        true
    }

    pub fn submit_capture(&mut self, value: String) {
        match self.phase {
            KnownPhase::EnvName => {
                self.env_name = value.trim().to_owned();
                self.phase = KnownPhase::Models;
            }
            KnownPhase::Secret => {
                self.secret = value;
                self.phase = KnownPhase::Models;
            }
            _ => {}
        }
        self.selected = 0;
    }

    pub fn confirm(&mut self) -> SetupStepOutcome {
        match self.phase {
            KnownPhase::Credential => {
                self.use_env = self.selected == 0;
                if self.use_env {
                    self.phase = KnownPhase::EnvName;
                    SetupStepOutcome::Capture(CaptureSpec {
                        label: "environment variable".into(),
                        initial: self.env_name.clone(),
                        masked: false,
                    })
                } else {
                    self.phase = KnownPhase::Secret;
                    SetupStepOutcome::Capture(CaptureSpec {
                        label: "API key".into(),
                        initial: String::new(),
                        masked: true,
                    })
                }
            }
            KnownPhase::EnvName | KnownPhase::Secret => SetupStepOutcome::None,
            KnownPhase::Models => {
                if self.selected >= self.kind.models.len() {
                    if !self.enabled.is_empty() {
                        self.phase = KnownPhase::DefaultModel;
                        self.selected = 0;
                    }
                } else if let Some(model) = self.kind.models.get(self.selected) {
                    if !self.enabled.remove(&model.id) {
                        self.enabled.insert(model.id.clone());
                    }
                    if !self.enabled.contains(&self.default_model) {
                        self.default_model =
                            self.enabled.iter().next().cloned().unwrap_or_default();
                    }
                }
                SetupStepOutcome::None
            }
            KnownPhase::DefaultModel => {
                let available: Vec<&str> = self
                    .kind
                    .models
                    .iter()
                    .filter(|model| self.enabled.contains(&model.id))
                    .map(|model| model.id.as_str())
                    .collect();
                if let Some(model) = available.get(self.selected) {
                    self.default_model = (*model).to_owned();
                    self.phase = KnownPhase::Review;
                    self.selected = 0;
                }
                SetupStepOutcome::None
            }
            KnownPhase::Review => {
                if self.selected == 1 {
                    return SetupStepOutcome::Cancel;
                }
                let credential = if self.use_env {
                    SetupCredential::Env(self.env_name.clone())
                } else {
                    SetupCredential::Secret(self.secret.clone())
                };
                SetupStepOutcome::Apply(SetupPlan::Apply {
                    name: self.kind.kind.clone(),
                    provider_kind: self.kind.kind.clone(),
                    base_url: None,
                    credential,
                    model: self.default_model.clone(),
                    enabled_models: Some(self.enabled.iter().cloned().collect()),
                    effort: ReasoningEffort::ProviderDefault,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::CatalogModel;

    fn kind() -> SetupKind {
        SetupKind {
            kind: "deepseek".into(),
            label: "DeepSeek".into(),
            default_base_url: "https://api.deepseek.com".into(),
            requires_base_url: false,
            credential_label: "env:DEEPSEEK_API_KEY".into(),
            default_model: "deepseek-flash".into(),
            models: ["deepseek-flash", "deepseek-v4-pro"]
                .into_iter()
                .map(|id| CatalogModel {
                    id: id.into(),
                    display_name: id.into(),
                    efforts: vec![],
                    default_effort: ReasoningEffort::ProviderDefault,
                    input_modalities: vec![],
                })
                .collect(),
        }
    }

    #[test]
    fn known_flow_collects_credential_models_default_and_save() {
        let mut flow = KnownProviderFlow::new(kind());
        assert_eq!(flow.phase(), KnownPhase::Credential);
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("DEEPSEEK_API_KEY".into());
        assert_eq!(flow.phase(), KnownPhase::Models);
        flow.down();
        flow.confirm(); // enable the second model
        flow.down();
        flow.confirm(); // continue
        assert_eq!(flow.phase(), KnownPhase::DefaultModel);
        flow.down();
        flow.confirm();
        assert_eq!(flow.phase(), KnownPhase::Review);
        let SetupStepOutcome::Apply(SetupPlan::Apply {
            model,
            enabled_models,
            base_url,
            ..
        }) = flow.confirm()
        else {
            panic!("expected save");
        };
        assert_eq!(model, "deepseek-v4-pro");
        assert_eq!(enabled_models.unwrap().len(), 2);
        assert!(base_url.is_none());
    }

    #[test]
    fn secret_never_enters_rows_review_or_debug() {
        let mut flow = KnownProviderFlow::new(kind());
        flow.down();
        let SetupStepOutcome::Capture(spec) = flow.confirm() else {
            panic!()
        };
        assert!(spec.masked);
        flow.submit_capture("sk-private".into());
        assert!(!format!("{flow:?}").contains("sk-private"));
        assert!(!format!("{:?}", flow.review_lines()).contains("sk-private"));
        assert!(!format!("{:?}", flow.rows()).contains("sk-private"));
    }
}
