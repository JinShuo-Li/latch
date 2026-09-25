//! Custom-provider setup: protocol, base URL, credential, one custom model,
//! and Save. Known providers use [`super::KnownProviderFlow`] instead; both
//! are single staged transactions and neither asks for Advanced-only fields.

use crate::profile::{
    CaptureSpec, ChoiceRow, SetupCredential, SetupKind, SetupPlan, SetupStepOutcome,
};
use latch_protocol::ReasoningEffort;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomPhase {
    BaseUrl,
    Protocol,
    Credential,
    EnvName,
    Secret,
    ModelId,
    ModelName,
    Review,
}

/// One custom provider as it is configured for the first time.
#[derive(Clone)]
pub struct CustomProviderFlow {
    kind: SetupKind,
    phase: CustomPhase,
    selected: usize,
    base_url: String,
    transport: String,
    use_env: bool,
    env_name: String,
    secret: String,
    model: String,
    display_name: String,
}

impl std::fmt::Debug for CustomProviderFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomProviderFlow")
            .field("phase", &self.phase)
            .field("base_url", &self.base_url)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

const PROTOCOLS: [(&str, &str, &str); 4] = [
    (
        "chat_completions",
        "Chat completions",
        "OpenAI-compatible /chat/completions",
    ),
    ("responses", "Responses", "OpenAI /responses"),
    ("anthropic_messages", "Messages", "Anthropic /v1/messages"),
    (
        "gemini",
        "Gemini",
        "generative language streamGenerateContent",
    ),
];

impl CustomProviderFlow {
    pub fn new(kind: SetupKind) -> Self {
        let base_url = kind.default_base_url.clone();
        let env_name = kind.credential_label.trim_start_matches("env:").to_owned();
        Self {
            kind,
            phase: CustomPhase::BaseUrl,
            selected: 0,
            base_url,
            transport: "chat_completions".to_owned(),
            use_env: true,
            env_name,
            secret: String::new(),
            model: String::new(),
            display_name: String::new(),
        }
    }

    pub fn phase(&self) -> CustomPhase {
        self.phase
    }

    pub fn title(&self) -> String {
        match self.phase {
            CustomPhase::BaseUrl => "Setup · Custom Provider · Base URL".to_owned(),
            CustomPhase::Protocol => "Setup · Custom Provider · Protocol".to_owned(),
            CustomPhase::Credential | CustomPhase::EnvName | CustomPhase::Secret => {
                "Setup · Custom Provider · Credential".to_owned()
            }
            CustomPhase::ModelId => "Setup · Custom Provider · Model id".to_owned(),
            CustomPhase::ModelName => "Setup · Custom Provider · Display name".to_owned(),
            CustomPhase::Review => "Setup · Custom Provider · Save".to_owned(),
        }
    }

    pub fn rows(&self) -> Vec<ChoiceRow> {
        let labels: Vec<(String, String, bool)> = match self.phase {
            CustomPhase::BaseUrl => vec![(
                format!("Base URL: {}", self.base_url),
                "press enter to edit".to_owned(),
                false,
            )],
            CustomPhase::Protocol => PROTOCOLS
                .iter()
                .map(|(value, label, description)| {
                    (
                        (*label).to_owned(),
                        (*description).to_owned(),
                        *value == self.transport,
                    )
                })
                .collect(),
            CustomPhase::Credential => vec![
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
            CustomPhase::ModelId | CustomPhase::ModelName => Vec::new(),
            CustomPhase::EnvName | CustomPhase::Secret => Vec::new(),
            CustomPhase::Review => vec![
                (
                    "Save provider".into(),
                    "config and secret staged".into(),
                    false,
                ),
                ("Cancel".into(), String::new(), false),
            ],
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
            ("Provider".into(), "Custom Provider".into()),
            ("Protocol".into(), self.transport.clone()),
            ("Base URL".into(), self.base_url.clone()),
            (
                "Credential".into(),
                if self.use_env {
                    format!("env:{}", self.env_name)
                } else {
                    "secure local storage (value hidden)".into()
                },
            ),
            ("Model".into(), self.model.clone()),
            ("Display name".into(), self.display_name.clone()),
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
            CustomPhase::BaseUrl => return false,
            CustomPhase::Protocol => CustomPhase::BaseUrl,
            CustomPhase::Credential => CustomPhase::Protocol,
            CustomPhase::EnvName | CustomPhase::Secret => CustomPhase::Credential,
            CustomPhase::ModelId => CustomPhase::Credential,
            CustomPhase::ModelName => CustomPhase::ModelId,
            CustomPhase::Review => CustomPhase::ModelName,
        };
        self.selected = 0;
        true
    }

    pub fn submit_capture(&mut self, value: String) {
        match self.phase {
            CustomPhase::BaseUrl => {
                let base_url = value.trim().to_owned();
                if !base_url.is_empty() {
                    self.base_url = base_url;
                }
                self.phase = CustomPhase::Protocol;
            }
            CustomPhase::EnvName => {
                self.env_name = value.trim().to_owned();
                self.phase = CustomPhase::ModelId;
            }
            CustomPhase::Secret => {
                self.secret = value;
                self.phase = CustomPhase::ModelId;
            }
            CustomPhase::ModelId => {
                self.model = value.trim().to_owned();
                self.display_name = self.model.clone();
                self.phase = CustomPhase::ModelName;
            }
            CustomPhase::ModelName => {
                let display_name = value.trim().to_owned();
                if !display_name.is_empty() {
                    self.display_name = display_name;
                }
                self.phase = CustomPhase::Review;
            }
            _ => {}
        }
        self.selected = 0;
    }

    pub fn confirm(&mut self) -> SetupStepOutcome {
        match self.phase {
            CustomPhase::BaseUrl => {
                self.phase = CustomPhase::BaseUrl;
                SetupStepOutcome::Capture(CaptureSpec {
                    label: "base URL".into(),
                    initial: self.base_url.clone(),
                    masked: false,
                })
            }
            CustomPhase::Protocol => {
                if let Some((value, _, _)) = PROTOCOLS.get(self.selected) {
                    self.transport = (*value).to_owned();
                    self.phase = CustomPhase::Credential;
                    self.selected = 0;
                }
                SetupStepOutcome::None
            }
            CustomPhase::Credential => {
                self.use_env = self.selected == 0;
                if self.use_env {
                    self.phase = CustomPhase::EnvName;
                    SetupStepOutcome::Capture(CaptureSpec {
                        label: "environment variable".into(),
                        initial: self.env_name.clone(),
                        masked: false,
                    })
                } else {
                    self.phase = CustomPhase::Secret;
                    SetupStepOutcome::Capture(CaptureSpec {
                        label: "API key".into(),
                        initial: String::new(),
                        masked: true,
                    })
                }
            }
            CustomPhase::EnvName | CustomPhase::Secret => SetupStepOutcome::None,
            CustomPhase::ModelId => {
                self.phase = CustomPhase::ModelId;
                SetupStepOutcome::Capture(CaptureSpec {
                    label: "model request id".into(),
                    initial: self.model.clone(),
                    masked: false,
                })
            }
            CustomPhase::ModelName => {
                self.phase = CustomPhase::ModelName;
                SetupStepOutcome::Capture(CaptureSpec {
                    label: "display name".into(),
                    initial: self.display_name.clone(),
                    masked: false,
                })
            }
            CustomPhase::Review => {
                if self.selected == 1 {
                    return SetupStepOutcome::Cancel;
                }
                if self.base_url.trim().is_empty() || self.model.trim().is_empty() {
                    return SetupStepOutcome::None;
                }
                let credential = if self.use_env {
                    SetupCredential::Env(self.env_name.clone())
                } else {
                    SetupCredential::Secret(self.secret.clone())
                };
                SetupStepOutcome::Apply(SetupPlan::Apply {
                    name: self.kind.kind.clone(),
                    provider_kind: self.kind.kind.clone(),
                    base_url: Some(self.base_url.clone()),
                    credential,
                    model: self.model.clone(),
                    enabled_models: Some(vec![self.model.clone()]),
                    custom_model_display_name: Some(self.display_name.clone()),
                    custom_transport: Some(self.transport.clone()),
                    effort: ReasoningEffort::ProviderDefault,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind() -> SetupKind {
        SetupKind {
            kind: "openai-compatible".into(),
            label: "Custom".into(),
            default_base_url: String::new(),
            requires_base_url: true,
            credential_label: "env:OPENAI_API_KEY".into(),
            default_model: String::new(),
            models: vec![],
        }
    }

    #[test]
    fn custom_flow_collects_protocol_base_url_credential_model_and_saves() {
        let mut flow = CustomProviderFlow::new(kind());
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("https://example.com/v1".into());
        assert_eq!(flow.phase(), CustomPhase::Protocol);
        flow.down();
        flow.confirm(); // responses
        assert_eq!(flow.phase(), CustomPhase::Credential);
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("EXAMPLE_KEY".into());
        assert_eq!(flow.phase(), CustomPhase::ModelId);
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("acme-pro".into());
        assert_eq!(flow.phase(), CustomPhase::ModelName);
        assert!(matches!(flow.confirm(), SetupStepOutcome::Capture(_)));
        flow.submit_capture("Acme Pro".into());
        assert_eq!(flow.phase(), CustomPhase::Review);
        let SetupStepOutcome::Apply(SetupPlan::Apply {
            provider_kind,
            base_url,
            model,
            custom_transport,
            custom_model_display_name,
            ..
        }) = flow.confirm()
        else {
            panic!("expected save");
        };
        assert_eq!(provider_kind, "openai-compatible");
        assert_eq!(base_url.as_deref(), Some("https://example.com/v1"));
        assert_eq!(model, "acme-pro");
        assert_eq!(custom_transport.as_deref(), Some("responses"));
        assert_eq!(custom_model_display_name.as_deref(), Some("Acme Pro"));
    }

    #[test]
    fn secret_never_enters_rows_review_or_debug() {
        let mut flow = CustomProviderFlow::new(kind());
        flow.confirm();
        flow.submit_capture("https://example.com/v1".into());
        flow.confirm(); // protocol default
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
