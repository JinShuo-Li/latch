//! Provider-list navigation state for `/setup`.
//!
//! This layer contains display state and actions only. Credential lookup,
//! model discovery, and persistence belong to the CLI/kernel. Every editing
//! surface emits one field-level action so unrelated overrides are preserved
//! byte for byte.

use crate::profile::{
    CaptureSpec, ChoiceRow, EffortMapEdit, ModelFieldEdit, ProviderFieldEdit, SetupCredential,
    SetupKind,
};
use latch_protocol::ReasoningEffort;
use std::collections::{BTreeMap, BTreeSet};
mod custom;
mod known;
pub use custom::{CustomPhase, CustomProviderFlow};
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

#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
            input_modalities: Vec::new(),
            input_modalities_configured: false,
            aliases: Vec::new(),
            aliases_configured: false,
            efforts: Vec::new(),
            default_effort: ReasoningEffort::ProviderDefault,
            has_override: false,
            effort_map: BTreeMap::new(),
            catalog_transport: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CenterPage {
    Providers,
    AddKind,
    Provider(String),
    Credential(String),
    Models(String),
    DefaultModel(String),
    Advanced(String),
    ModelAdvanced {
        provider: String,
        model: String,
    },
    ModelChoice {
        provider: String,
        model: String,
        kind: ModelChoiceKind,
    },
    Efforts {
        provider: String,
        model: String,
    },
    EffortMap {
        provider: String,
        model: String,
    },
    EffortMapForm {
        provider: String,
        model: String,
        effort: ReasoningEffort,
    },
    RemoveConfirm(String),
}

/// Which enumerated model field a choice page edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelChoiceKind {
    Transport,
    ReasoningReplay,
    AdaptiveThinking,
    InputModalities,
    DefaultEffort,
}

impl ModelChoiceKind {
    const fn title(self) -> &'static str {
        match self {
            Self::Transport => "Transport",
            Self::ReasoningReplay => "Reasoning replay",
            Self::AdaptiveThinking => "Adaptive thinking",
            Self::InputModalities => "Input modalities",
            Self::DefaultEffort => "Default effort",
        }
    }
}

/// One pending text capture. `capture_spec()` turns it into the composer
/// capture the host opens; `submit_capture` consumes the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CenterCapture {
    CredentialEnv {
        provider: String,
    },
    CredentialSecret {
        provider: String,
    },
    BaseUrl {
        provider: String,
    },
    ProviderName {
        provider: String,
    },
    CustomModelId {
        provider: String,
    },
    CustomModelName {
        provider: String,
        model: String,
    },
    ModelDisplayName {
        provider: String,
        model: String,
    },
    ContextWindow {
        provider: String,
        model: String,
    },
    Aliases {
        provider: String,
        model: String,
    },
    EffortValue {
        provider: String,
        model: String,
        effort: ReasoningEffort,
    },
    EffortBudget {
        provider: String,
        model: String,
        effort: ReasoningEffort,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CenterAction {
    StartAdd(String),
    /// The host must open `ConfigurationCenter::capture_spec()`.
    Capture,
    SetCredential {
        name: String,
        credential: SetupCredential,
    },
    SetEnabledModels {
        name: String,
        models: Vec<String>,
    },
    AddCustomModel {
        name: String,
        model: String,
        display_name: String,
    },
    SetProviderField {
        name: String,
        field: ProviderFieldEdit,
    },
    SetModelField {
        name: String,
        model: String,
        field: ModelFieldEdit,
    },
    DiscoverModels(String),
    SetProviderDefault {
        name: String,
        model: String,
    },
    SetNewSessionDefault(String),
    Remove(String),
}

/// In-progress sequential effort-map edit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EffortDraft {
    provider: String,
    model: String,
    levels: Vec<ReasoningEffort>,
    map: BTreeMap<ReasoningEffort, EffortMapEdit>,
}

#[derive(Debug, Clone)]
pub struct ConfigurationCenter {
    providers: Vec<ProviderSummary>,
    kinds: Vec<SetupKind>,
    page: CenterPage,
    selected: usize,
    draft_models: BTreeSet<String>,
    draft_efforts: BTreeSet<ReasoningEffort>,
    capture: Option<CenterCapture>,
    /// Request id between the custom-model id and display-name captures.
    pending_model: Option<String>,
    effort_draft: Option<EffortDraft>,
}

impl ConfigurationCenter {
    pub fn new(providers: Vec<ProviderSummary>, kinds: Vec<SetupKind>) -> Self {
        Self {
            providers,
            kinds,
            page: CenterPage::Providers,
            selected: 0,
            draft_models: BTreeSet::new(),
            draft_efforts: BTreeSet::new(),
            capture: None,
            pending_model: None,
            effort_draft: None,
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
            CenterPage::Advanced(id) => format!("Setup · {id} · Advanced"),
            CenterPage::ModelAdvanced { provider, model } => {
                format!("Setup · {provider} · {model} · Advanced")
            }
            CenterPage::ModelChoice {
                provider,
                model,
                kind,
            } => format!("Setup · {provider} · {model} · {}", kind.title()),
            CenterPage::Efforts { provider, model } => {
                format!("Setup · {provider} · {model} · Efforts")
            }
            CenterPage::EffortMap { provider, model } => {
                format!("Setup · {provider} · {model} · Effort mapping")
            }
            CenterPage::EffortMapForm {
                provider,
                model,
                effort,
            } => format!("Setup · {provider} · {model} · {} form", effort.label()),
            CenterPage::RemoveConfirm(id) => format!("Setup · remove {id}"),
        }
    }

    fn provider(&self, id: &str) -> Option<&ProviderSummary> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    fn model(&self, provider: &str, model: &str) -> Option<&ProviderModelSummary> {
        self.provider(provider)?
            .models
            .iter()
            .find(|entry| entry.id == model)
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
                    .provider(id)
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
                        "base URL, transport, capabilities, effort mapping".to_owned(),
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
                    .provider(id)
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
            CenterPage::Models(id) => {
                let discoverable = self.provider(id).is_some_and(|provider| {
                    matches!(provider.kind.as_str(), "opencode-go" | "opencode-zen")
                });
                let Some(provider) = self.provider(id) else {
                    return Vec::new();
                };
                let mut rows: Vec<(String, String)> = provider
                    .models
                    .iter()
                    .map(|model| {
                        let status = if !model.resolved {
                            "unresolved"
                        } else if provider.discovered_ids.contains(&model.id) {
                            "available"
                        } else {
                            ""
                        };
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
                    .collect();
                rows.push((
                    "Add custom model…".to_owned(),
                    "request id and display name".to_owned(),
                ));
                if discoverable {
                    rows.push((
                        "Refresh available models".to_owned(),
                        "from provider".to_owned(),
                    ));
                }
                rows.push((
                    "Save model selection".to_owned(),
                    format!("{} selected", self.draft_models.len()),
                ));
                rows
            }
            CenterPage::DefaultModel(id) => self
                .provider(id)
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
            CenterPage::Advanced(id) => {
                let Some(provider) = self.provider(id) else {
                    return Vec::new();
                };
                let mut rows = vec![
                    ("Base URL".to_owned(), provider.base_url.clone()),
                    ("Provider name".to_owned(), provider.display_name.clone()),
                ];
                rows.extend(provider.models.iter().map(|model| {
                    let status = if !model.resolved {
                        "unresolved transport".to_owned()
                    } else {
                        model.transport.clone()
                    };
                    (
                        model.display_name.clone(),
                        format!("{} · {status}", model.id),
                    )
                }));
                rows
            }
            CenterPage::ModelAdvanced { provider, model } => {
                let Some(entry) = self.model(provider, model) else {
                    return Vec::new();
                };
                let efforts = if entry.efforts.is_empty() {
                    "none exposed".to_owned()
                } else {
                    entry
                        .efforts
                        .iter()
                        .map(|effort| effort.label())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let transport = if entry.transport_configured {
                    format!("{} (override)", entry.transport)
                } else {
                    format!("{} (provider default)", entry.transport)
                };
                let replay = entry
                    .reasoning_replay
                    .clone()
                    .unwrap_or_else(|| "provider default".to_owned());
                let context = entry
                    .context_window_tokens
                    .map_or_else(|| "catalog/default".to_owned(), |tokens| tokens.to_string());
                let modalities = if entry.input_modalities.is_empty() {
                    "text".to_owned()
                } else {
                    entry.input_modalities.join(", ")
                };
                let mapping = if entry.effort_map.is_empty() {
                    "automatic (adapter default)".to_owned()
                } else {
                    format!("custom ({} levels)", entry.effort_map.len())
                };
                let mut rows = vec![
                    ("Display name".to_owned(), entry.display_name.clone()),
                    ("Transport".to_owned(), transport),
                    ("Context window".to_owned(), context),
                    ("Reasoning replay".to_owned(), replay),
                    (
                        "Adaptive thinking".to_owned(),
                        if entry.adaptive_thinking_configured {
                            if entry.adaptive_thinking {
                                "on".to_owned()
                            } else {
                                "off".to_owned()
                            }
                        } else if entry.adaptive_thinking {
                            "on (catalog)".to_owned()
                        } else {
                            "off".to_owned()
                        },
                    ),
                    ("Input modalities".to_owned(), modalities),
                    ("Aliases".to_owned(), entry.aliases.join(", ")),
                    ("Effort levels".to_owned(), efforts),
                    (
                        "Default effort".to_owned(),
                        entry.default_effort.label().to_owned(),
                    ),
                    ("Effort mapping".to_owned(), mapping),
                ];
                rows.push(if entry.from_catalog {
                    (
                        "Reset model overrides".to_owned(),
                        "restore catalog metadata".to_owned(),
                    )
                } else {
                    (
                        "Remove custom model".to_owned(),
                        "also unselects it".to_owned(),
                    )
                });
                rows
            }
            CenterPage::ModelChoice {
                provider,
                model,
                kind,
            } => {
                let Some(entry) = self.model(provider, model) else {
                    return Vec::new();
                };
                match kind {
                    ModelChoiceKind::Transport => vec![
                        (
                            "Provider default".to_owned(),
                            if entry.catalog_transport.is_empty() {
                                "catalog".to_owned()
                            } else {
                                format!("catalog: {}", entry.catalog_transport)
                            },
                        ),
                        (
                            "Chat completions".to_owned(),
                            "openai-compatible".to_owned(),
                        ),
                        ("Responses".to_owned(), "openai responses".to_owned()),
                        ("Messages".to_owned(), "anthropic".to_owned()),
                        ("Gemini".to_owned(), "generative language".to_owned()),
                    ],
                    ModelChoiceKind::ReasoningReplay => vec![
                        ("Provider default".to_owned(), String::new()),
                        ("Replay".to_owned(), "send reasoning back".to_owned()),
                        ("Omit".to_owned(), "never replay reasoning".to_owned()),
                    ],
                    ModelChoiceKind::AdaptiveThinking => vec![
                        ("Provider default".to_owned(), String::new()),
                        ("On".to_owned(), "adaptive thinking".to_owned()),
                        ("Off".to_owned(), "classic thinking".to_owned()),
                    ],
                    ModelChoiceKind::InputModalities => vec![
                        ("Text only".to_owned(), String::new()),
                        ("Text and images".to_owned(), "vision".to_owned()),
                    ],
                    ModelChoiceKind::DefaultEffort => std::iter::once((
                        "Provider default".to_owned(),
                        "let the provider decide".to_owned(),
                    ))
                    .chain(entry.efforts.iter().map(|effort| {
                        (
                            effort.label().to_owned(),
                            if *effort == entry.default_effort {
                                "current".to_owned()
                            } else {
                                String::new()
                            },
                        )
                    }))
                    .collect(),
                }
            }
            CenterPage::Efforts { provider, model } => {
                let Some(entry) = self.model(provider, model) else {
                    return Vec::new();
                };
                let mut rows: Vec<(String, String)> = ReasoningEffort::LEVELS
                    .iter()
                    .map(|effort| {
                        (
                            format!(
                                "{} {}",
                                if self.draft_efforts.contains(effort) {
                                    "[x]"
                                } else {
                                    "[ ]"
                                },
                                effort.label()
                            ),
                            String::new(),
                        )
                    })
                    .collect();
                rows.push((
                    "Save effort levels".to_owned(),
                    format!("{} exposed", self.draft_efforts.len()),
                ));
                let _ = entry;
                rows
            }
            CenterPage::EffortMap { provider, model } => {
                let Some(entry) = self.model(provider, model) else {
                    return Vec::new();
                };
                let draft = self
                    .effort_draft
                    .as_ref()
                    .filter(|draft| draft.provider == *provider && draft.model == *model);
                let mut rows: Vec<(String, String)> = entry
                    .efforts
                    .iter()
                    .map(|effort| {
                        let form = draft
                            .and_then(|draft| draft.map.get(effort))
                            .or_else(|| entry.effort_map.get(effort));
                        let label = match form {
                            Some(EffortMapEdit::Automatic) | None => "automatic",
                            Some(EffortMapEdit::Value(_)) => "value",
                            Some(EffortMapEdit::Budget(_)) => "budget_tokens",
                            Some(EffortMapEdit::Disabled) => "disabled",
                        };
                        (effort.label().to_owned(), label.to_owned())
                    })
                    .collect();
                let complete = self.effort_draft.as_ref().is_some_and(|draft| {
                    draft.provider == *provider
                        && draft.model == *model
                        && entry.efforts.iter().all(|effort| {
                            matches!(
                                draft.map.get(effort),
                                Some(
                                    EffortMapEdit::Value(_)
                                        | EffortMapEdit::Budget(_)
                                        | EffortMapEdit::Disabled
                                )
                            )
                        })
                });
                rows.push((
                    "Save mapping".to_owned(),
                    if complete {
                        "covers every exposed effort".to_owned()
                    } else {
                        "set every exposed effort first".to_owned()
                    },
                ));
                rows.push((
                    "Clear mapping".to_owned(),
                    "use the adapter default".to_owned(),
                ));
                rows
            }
            CenterPage::EffortMapForm {
                provider,
                model,
                effort,
            } => {
                let _ = self.model(provider, model);
                vec![
                    ("Automatic".to_owned(), complete_effort(effort)),
                    ("Value".to_owned(), value_hint(effort)),
                    ("Budget tokens".to_owned(), budget_hint(effort)),
                    ("Disabled".to_owned(), "documented off switch".to_owned()),
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
            CenterPage::DefaultModel(id) => CenterPage::Provider(id.clone()),
            CenterPage::Credential(id) => CenterPage::Provider(id.clone()),
            CenterPage::Models(id) => CenterPage::Provider(id.clone()),
            CenterPage::Advanced(id) => CenterPage::Provider(id.clone()),
            CenterPage::ModelAdvanced { provider, .. } => CenterPage::Advanced(provider.clone()),
            CenterPage::ModelChoice {
                provider, model, ..
            } => CenterPage::ModelAdvanced {
                provider: provider.clone(),
                model: model.clone(),
            },
            CenterPage::Efforts { provider, model } => CenterPage::ModelAdvanced {
                provider: provider.clone(),
                model: model.clone(),
            },
            CenterPage::EffortMap { provider, model } => CenterPage::ModelAdvanced {
                provider: provider.clone(),
                model: model.clone(),
            },
            CenterPage::EffortMapForm {
                provider, model, ..
            } => CenterPage::EffortMap {
                provider: provider.clone(),
                model: model.clone(),
            },
            CenterPage::RemoveConfirm(id) => CenterPage::Provider(id.clone()),
        };
        self.selected = 0;
        true
    }

    pub fn confirm(&mut self) -> Option<CenterAction> {
        match self.page.clone() {
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
            CenterPage::Provider(id) => match self.selected {
                0 => {
                    self.page = CenterPage::Credential(id);
                    self.selected = 0;
                    None
                }
                1 => {
                    self.draft_models = self
                        .provider(&id)?
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
                3 => {
                    self.page = CenterPage::Advanced(id);
                    self.selected = 0;
                    None
                }
                4 => Some(CenterAction::SetNewSessionDefault(id)),
                _ => {
                    self.page = CenterPage::RemoveConfirm(id);
                    self.selected = 0;
                    None
                }
            },
            CenterPage::RemoveConfirm(id) => {
                if self.selected == 0 {
                    Some(CenterAction::Remove(id))
                } else {
                    self.back();
                    None
                }
            }
            CenterPage::Credential(id) => {
                self.capture = Some(if self.selected == 0 {
                    CenterCapture::CredentialEnv { provider: id }
                } else {
                    CenterCapture::CredentialSecret { provider: id }
                });
                Some(CenterAction::Capture)
            }
            CenterPage::Models(id) => {
                let (model_count, default_model, discoverable) = {
                    let provider = self.provider(&id)?;
                    (
                        provider.models.len(),
                        provider.default_model.clone(),
                        matches!(provider.kind.as_str(), "opencode-go" | "opencode-zen"),
                    )
                };
                if self.selected < model_count {
                    let (model_id, resolved, is_default) = {
                        let model = self.provider(&id)?.models.get(self.selected)?;
                        (model.id.clone(), model.resolved, model.id == default_model)
                    };
                    if !resolved || is_default {
                        return None;
                    }
                    if !self.draft_models.remove(&model_id) {
                        self.draft_models.insert(model_id);
                    }
                    return None;
                }
                let add_row = model_count;
                if self.selected == add_row {
                    self.capture = Some(CenterCapture::CustomModelId {
                        provider: id.clone(),
                    });
                    return Some(CenterAction::Capture);
                }
                if discoverable && self.selected == add_row + 1 {
                    return Some(CenterAction::DiscoverModels(id));
                }
                if self.draft_models.is_empty() || !self.draft_models.contains(&default_model) {
                    return None;
                }
                Some(CenterAction::SetEnabledModels {
                    name: id,
                    models: self.draft_models.iter().cloned().collect(),
                })
            }
            CenterPage::DefaultModel(id) => Some(CenterAction::SetProviderDefault {
                name: id.clone(),
                model: self
                    .provider(&id)?
                    .available_models
                    .get(self.selected)?
                    .clone(),
            }),
            CenterPage::Advanced(id) => {
                if self.selected == 0 {
                    self.capture = Some(CenterCapture::BaseUrl { provider: id });
                    return Some(CenterAction::Capture);
                }
                if self.selected == 1 {
                    self.capture = Some(CenterCapture::ProviderName { provider: id });
                    return Some(CenterAction::Capture);
                }
                let model = self
                    .provider(&id)?
                    .models
                    .get(self.selected - 2)?
                    .id
                    .clone();
                self.page = CenterPage::ModelAdvanced {
                    provider: id,
                    model,
                };
                self.selected = 0;
                None
            }
            CenterPage::ModelAdvanced { provider, model } => match self.selected {
                0 => {
                    self.capture = Some(CenterCapture::ModelDisplayName { provider, model });
                    Some(CenterAction::Capture)
                }
                1 => {
                    self.page = CenterPage::ModelChoice {
                        provider,
                        model,
                        kind: ModelChoiceKind::Transport,
                    };
                    self.selected = 0;
                    None
                }
                2 => {
                    self.capture = Some(CenterCapture::ContextWindow { provider, model });
                    Some(CenterAction::Capture)
                }
                3 => {
                    self.page = CenterPage::ModelChoice {
                        provider,
                        model,
                        kind: ModelChoiceKind::ReasoningReplay,
                    };
                    self.selected = 0;
                    None
                }
                4 => {
                    self.page = CenterPage::ModelChoice {
                        provider,
                        model,
                        kind: ModelChoiceKind::AdaptiveThinking,
                    };
                    self.selected = 0;
                    None
                }
                5 => {
                    self.page = CenterPage::ModelChoice {
                        provider,
                        model,
                        kind: ModelChoiceKind::InputModalities,
                    };
                    self.selected = 0;
                    None
                }
                6 => {
                    self.capture = Some(CenterCapture::Aliases { provider, model });
                    Some(CenterAction::Capture)
                }
                7 => {
                    let entry = self.model(&provider, &model)?;
                    self.draft_efforts = entry.efforts.iter().copied().collect();
                    self.page = CenterPage::Efforts { provider, model };
                    self.selected = 0;
                    None
                }
                8 => {
                    self.page = CenterPage::ModelChoice {
                        provider,
                        model,
                        kind: ModelChoiceKind::DefaultEffort,
                    };
                    self.selected = 0;
                    None
                }
                9 => {
                    let entry = self.model(&provider, &model)?;
                    self.effort_draft = Some(EffortDraft {
                        provider: provider.clone(),
                        model: model.clone(),
                        levels: entry.efforts.clone(),
                        map: entry.effort_map.clone(),
                    });
                    self.page = CenterPage::EffortMap { provider, model };
                    self.selected = 0;
                    None
                }
                _ => Some(CenterAction::SetModelField {
                    name: provider,
                    model,
                    field: ModelFieldEdit::Reset,
                }),
            },
            CenterPage::ModelChoice {
                provider,
                model,
                kind,
            } => {
                let field = match kind {
                    ModelChoiceKind::Transport => {
                        Some(ModelFieldEdit::Transport(match self.selected {
                            0 => None,
                            1 => Some("chat_completions".to_owned()),
                            2 => Some("responses".to_owned()),
                            3 => Some("anthropic_messages".to_owned()),
                            _ => Some("gemini".to_owned()),
                        }))
                    }
                    ModelChoiceKind::ReasoningReplay => {
                        Some(ModelFieldEdit::ReasoningReplay(match self.selected {
                            0 => None,
                            1 => Some("replay".to_owned()),
                            _ => Some("omit".to_owned()),
                        }))
                    }
                    ModelChoiceKind::AdaptiveThinking => {
                        Some(ModelFieldEdit::AdaptiveThinking(match self.selected {
                            0 => None,
                            1 => Some(true),
                            _ => Some(false),
                        }))
                    }
                    ModelChoiceKind::InputModalities => {
                        Some(ModelFieldEdit::InputModalities(if self.selected == 0 {
                            vec!["text".to_owned()]
                        } else {
                            vec!["text".to_owned(), "image".to_owned()]
                        }))
                    }
                    ModelChoiceKind::DefaultEffort => {
                        let entry = self.model(&provider, &model)?;
                        Some(ModelFieldEdit::Efforts {
                            efforts: entry.efforts.clone(),
                            default_effort: entry
                                .efforts
                                .get(self.selected.checked_sub(1)?)
                                .copied()
                                .or(Some(ReasoningEffort::ProviderDefault)),
                        })
                    }
                };
                field.map(|field| CenterAction::SetModelField {
                    name: provider,
                    model,
                    field,
                })
            }
            CenterPage::Efforts { provider, model } => {
                let entry = self.model(&provider, &model)?;
                if self.selected < ReasoningEffort::LEVELS.len() {
                    let effort = ReasoningEffort::LEVELS[self.selected];
                    if !self.draft_efforts.remove(&effort) {
                        self.draft_efforts.insert(effort);
                    }
                    return None;
                }
                Some(CenterAction::SetModelField {
                    name: provider,
                    model,
                    field: ModelFieldEdit::Efforts {
                        efforts: ReasoningEffort::LEVELS
                            .iter()
                            .copied()
                            .filter(|effort| self.draft_efforts.contains(effort))
                            .collect(),
                        default_effort: Some(entry.default_effort),
                    },
                })
            }
            CenterPage::EffortMap { provider, model } => {
                let entry = self.model(&provider, &model)?;
                if self.selected < entry.efforts.len() {
                    let effort = entry.efforts[self.selected];
                    self.page = CenterPage::EffortMapForm {
                        provider,
                        model,
                        effort,
                    };
                    self.selected = 0;
                    return None;
                }
                if self.selected == entry.efforts.len() {
                    // Save: only a complete mapping is a valid configuration.
                    let draft = self.effort_draft.clone()?;
                    if entry
                        .efforts
                        .iter()
                        .any(|effort| !draft.map.contains_key(effort))
                    {
                        return None;
                    }
                    return Some(CenterAction::SetModelField {
                        name: provider,
                        model,
                        field: ModelFieldEdit::EffortMap(draft.map),
                    });
                }
                Some(CenterAction::SetModelField {
                    name: provider,
                    model,
                    field: ModelFieldEdit::EffortMap(BTreeMap::new()),
                })
            }
            CenterPage::EffortMapForm {
                provider,
                model,
                effort,
            } => {
                let form = match self.selected {
                    0 => EffortMapEdit::Automatic,
                    1 => {
                        self.capture = Some(CenterCapture::EffortValue {
                            provider: provider.clone(),
                            model: model.clone(),
                            effort,
                        });
                        return Some(CenterAction::Capture);
                    }
                    2 => {
                        self.capture = Some(CenterCapture::EffortBudget {
                            provider: provider.clone(),
                            model: model.clone(),
                            effort,
                        });
                        return Some(CenterAction::Capture);
                    }
                    _ => EffortMapEdit::Disabled,
                };
                if let Some(draft) = self.effort_draft.as_mut() {
                    if matches!(form, EffortMapEdit::Automatic) {
                        draft.map.remove(&effort);
                    } else {
                        draft.map.insert(effort, form);
                    }
                }
                self.page = CenterPage::EffortMap { provider, model };
                self.selected = 0;
                None
            }
        }
    }

    /// The capture the host must open for [`CenterAction::Capture`].
    pub fn capture_spec(&self) -> Option<CaptureSpec> {
        let capture = self.capture.as_ref()?;
        Some(match capture {
            CenterCapture::CredentialEnv { provider } => CaptureSpec {
                label: "environment variable".to_owned(),
                initial: self
                    .provider(provider)
                    .and_then(|provider| provider.credential_ref.strip_prefix("env:"))
                    .unwrap_or("")
                    .to_owned(),
                masked: false,
            },
            CenterCapture::CredentialSecret { .. } => CaptureSpec {
                label: "API key".to_owned(),
                initial: String::new(),
                masked: true,
            },
            CenterCapture::BaseUrl { provider } => CaptureSpec {
                label: "base URL".to_owned(),
                initial: self
                    .provider(provider)
                    .map(|provider| provider.base_url.clone())
                    .unwrap_or_default(),
                masked: false,
            },
            CenterCapture::ProviderName { provider } => CaptureSpec {
                label: "provider name".to_owned(),
                initial: self
                    .provider(provider)
                    .map(|provider| provider.display_name.clone())
                    .unwrap_or_default(),
                masked: false,
            },
            CenterCapture::CustomModelId { .. } => CaptureSpec {
                label: "model request id".to_owned(),
                initial: String::new(),
                masked: false,
            },
            CenterCapture::CustomModelName { model, .. } => CaptureSpec {
                label: "display name".to_owned(),
                initial: model.clone(),
                masked: false,
            },
            CenterCapture::ModelDisplayName { provider, model } => CaptureSpec {
                label: "display name".to_owned(),
                initial: self
                    .model(provider, model)
                    .map(|entry| entry.display_name.clone())
                    .unwrap_or_default(),
                masked: false,
            },
            CenterCapture::ContextWindow { provider, model } => CaptureSpec {
                label: "context window (tokens)".to_owned(),
                initial: self
                    .model(provider, model)
                    .and_then(|entry| entry.context_window_tokens)
                    .map(|tokens| tokens.to_string())
                    .unwrap_or_default(),
                masked: false,
            },
            CenterCapture::Aliases { provider, model } => CaptureSpec {
                label: "aliases (comma separated)".to_owned(),
                initial: self
                    .model(provider, model)
                    .map(|entry| entry.aliases.join(", "))
                    .unwrap_or_default(),
                masked: false,
            },
            CenterCapture::EffortValue { effort, .. } => CaptureSpec {
                label: format!("{} value", effort.label()),
                initial: String::new(),
                masked: false,
            },
            CenterCapture::EffortBudget { effort, .. } => CaptureSpec {
                label: format!("{} budget tokens", effort.label()),
                initial: String::new(),
                masked: false,
            },
        })
    }

    /// Consumes one captured value. Returns an action, another capture, or
    /// `None` when the value only advanced internal state.
    pub fn submit_capture(&mut self, value: String) -> Option<CenterAction> {
        let capture = self.capture.take()?;
        match capture {
            CenterCapture::CredentialEnv { provider } => Some(CenterAction::SetCredential {
                name: provider,
                credential: SetupCredential::Env(value.trim().to_owned()),
            }),
            CenterCapture::CredentialSecret { provider } => Some(CenterAction::SetCredential {
                name: provider,
                credential: SetupCredential::Secret(value),
            }),
            CenterCapture::BaseUrl { provider } => Some(CenterAction::SetProviderField {
                name: provider,
                field: ProviderFieldEdit::BaseUrl(
                    Some(value.trim().to_owned()).filter(|url| !url.is_empty()),
                ),
            }),
            CenterCapture::ProviderName { provider } => Some(CenterAction::SetProviderField {
                name: provider,
                field: ProviderFieldEdit::DisplayName(
                    Some(value.trim().to_owned()).filter(|name| !name.is_empty()),
                ),
            }),
            CenterCapture::CustomModelId { provider } => {
                let model = value.trim().to_owned();
                if model.is_empty() {
                    return None;
                }
                self.pending_model = Some(model.clone());
                self.capture = Some(CenterCapture::CustomModelName {
                    provider: provider.clone(),
                    model: model.clone(),
                });
                Some(CenterAction::Capture)
            }
            CenterCapture::CustomModelName { provider, model } => {
                let display_name = value.trim().to_owned();
                let display_name = if display_name.is_empty() {
                    model.clone()
                } else {
                    display_name
                };
                self.pending_model = None;
                Some(CenterAction::AddCustomModel {
                    name: provider,
                    model,
                    display_name,
                })
            }
            CenterCapture::ModelDisplayName { provider, model } => {
                Some(CenterAction::SetModelField {
                    name: provider,
                    model: model.clone(),
                    field: ModelFieldEdit::DisplayName(
                        Some(value.trim().to_owned()).filter(|name| !name.is_empty()),
                    ),
                })
            }
            CenterCapture::ContextWindow { provider, model } => {
                let parsed = value.trim().parse::<usize>().ok();
                Some(CenterAction::SetModelField {
                    name: provider,
                    model,
                    field: ModelFieldEdit::ContextWindow(parsed.filter(|tokens| *tokens > 0)),
                })
            }
            CenterCapture::Aliases { provider, model } => {
                let aliases = value
                    .split(',')
                    .map(str::trim)
                    .filter(|alias| !alias.is_empty())
                    .map(str::to_owned)
                    .collect();
                Some(CenterAction::SetModelField {
                    name: provider,
                    model,
                    field: ModelFieldEdit::Aliases(aliases),
                })
            }
            CenterCapture::EffortValue {
                provider,
                model,
                effort,
            } => {
                let value = value.trim().to_owned();
                if value.is_empty() {
                    return None;
                }
                if let Some(draft) = self.effort_draft.as_mut() {
                    draft.map.insert(effort, EffortMapEdit::Value(value));
                }
                self.page = CenterPage::EffortMap { provider, model };
                self.selected = 0;
                None
            }
            CenterCapture::EffortBudget {
                provider,
                model,
                effort,
            } => {
                let budget = value.trim().parse::<u64>().ok()?;
                if budget == 0 {
                    return None;
                }
                if let Some(draft) = self.effort_draft.as_mut() {
                    draft.map.insert(effort, EffortMapEdit::Budget(budget));
                }
                self.page = CenterPage::EffortMap { provider, model };
                self.selected = 0;
                None
            }
        }
    }

    pub fn credential_from_capture(&self, value: String) -> Option<(String, SetupCredential)> {
        let CenterCapture::CredentialEnv { provider } = self.capture.as_ref()? else {
            return None;
        };
        Some((
            provider.clone(),
            SetupCredential::Env(value.trim().to_owned()),
        ))
    }

    pub fn merge_discovered(&mut self, provider: &str, ids: &[String]) {
        if let Some(row) = self.providers.iter_mut().find(|row| row.id == provider) {
            row.merge_discovered(ids);
        }
    }

    /// Replaces provider data after a persisted edit while keeping the current
    /// page and selection. A page whose provider or model disappeared falls
    /// back to the provider list.
    pub fn set_providers(&mut self, providers: Vec<ProviderSummary>) {
        self.providers = providers;
        let valid = match &self.page {
            CenterPage::Providers | CenterPage::AddKind => true,
            CenterPage::Provider(id)
            | CenterPage::Credential(id)
            | CenterPage::Models(id)
            | CenterPage::DefaultModel(id)
            | CenterPage::Advanced(id)
            | CenterPage::RemoveConfirm(id) => self.provider(id).is_some(),
            CenterPage::ModelAdvanced { provider, model } => self.model(provider, model).is_some(),
            CenterPage::ModelChoice {
                provider, model, ..
            } => self.model(provider, model).is_some(),
            CenterPage::Efforts { provider, model } => self.model(provider, model).is_some(),
            CenterPage::EffortMap { provider, model } => self.model(provider, model).is_some(),
            CenterPage::EffortMapForm {
                provider, model, ..
            } => self.model(provider, model).is_some(),
        };
        if !valid {
            self.page = CenterPage::Providers;
        }
        let len = self.rows().len();
        if len > 0 {
            self.selected = self.selected.min(len - 1);
        }
        self.effort_draft = None;
    }

    /// Forgets a pending capture the user cancelled.
    pub fn cancel_capture(&mut self) {
        self.capture = None;
    }
}

fn value_hint(effort: &ReasoningEffort) -> String {
    format!("transport effort field for {}", effort.label())
}

fn budget_hint(effort: &ReasoningEffort) -> String {
    format!("token budget for {}", effort.label())
}

fn complete_effort(_effort: &ReasoningEffort) -> String {
    "adapter default".to_owned()
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

    fn provider(status: ProviderStatus) -> ProviderSummary {
        ProviderSummary {
            id: "deepseek".into(),
            display_name: "DeepSeek".into(),
            kind: "deepseek".into(),
            status,
            model_count: 1,
            default_model: "deepseek-flash".into(),
            credential_ref: "env:DEEPSEEK_API_KEY".into(),
            base_url: "https://api.deepseek.com".into(),
            available_models: vec!["deepseek-flash".into()],
            models: vec![ProviderModelSummary {
                id: "deepseek-flash".into(),
                display_name: "DeepSeek Flash".into(),
                enabled: true,
                resolved: true,
                from_catalog: true,
                transport: "chat completions".into(),
                catalog_transport: "chat completions".into(),
                input_modalities: vec!["text".into()],
                ..ProviderModelSummary::default()
            }],
            discovered_ids: vec![],
        }
    }

    fn open_provider(center: &mut ConfigurationCenter) {
        center.down();
        center.confirm();
    }

    #[test]
    fn first_run_highlights_add_and_list_uses_status_rows() {
        let empty = ConfigurationCenter::new(vec![], vec![kind()]);
        assert_eq!(empty.page(), &CenterPage::Providers);
        assert!(empty.rows()[0].selected);
        let mut center = ConfigurationCenter::new(
            vec![provider(ProviderStatus::MissingCredential)],
            vec![kind()],
        );
        assert!(
            center.rows()[1]
                .description
                .contains("missing credential · 1 models")
        );
        open_provider(&mut center);
        assert_eq!(center.page(), &CenterPage::Provider("deepseek".into()));
        assert_eq!(center.rows()[0].description, "env:DEEPSEEK_API_KEY");
        center.down();
        assert_eq!(center.confirm(), None);
        assert_eq!(center.page(), &CenterPage::Models("deepseek".into()));
    }

    #[test]
    fn credential_editor_captures_without_echoing_a_stored_value() {
        let mut center = ConfigurationCenter::new(
            vec![provider(ProviderStatus::MissingCredential)],
            vec![kind()],
        );
        open_provider(&mut center);
        center.confirm();
        assert_eq!(center.page(), &CenterPage::Credential("deepseek".into()));
        assert_eq!(center.confirm(), Some(CenterAction::Capture));
        assert_eq!(center.capture_spec().unwrap().label, "environment variable");
        assert_eq!(
            center.submit_capture("DEEPSEEK_API_KEY".into()),
            Some(CenterAction::SetCredential {
                name: "deepseek".into(),
                credential: SetupCredential::Env("DEEPSEEK_API_KEY".into()),
            })
        );
        // A secret capture is masked and never present in debug output.
        center.confirm();
        center.down();
        assert_eq!(center.confirm(), Some(CenterAction::Capture));
        let spec = center.capture_spec().unwrap();
        assert!(spec.masked);
        assert_eq!(
            center.submit_capture("private-value".into()),
            Some(CenterAction::SetCredential {
                name: "deepseek".into(),
                credential: SetupCredential::Secret("private-value".into()),
            })
        );
        assert!(!format!("{center:?}").contains("private-value"));
    }

    #[test]
    fn models_page_adds_custom_models_and_refreshes_discovery() {
        let mut provider = provider(ProviderStatus::Ready);
        provider.kind = "opencode-zen".into();
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        open_provider(&mut center);
        center.down();
        center.confirm();
        assert_eq!(center.page(), &CenterPage::Models("deepseek".into()));
        // [0] existing model, [1] add custom, [2] refresh, [3] save.
        center.down();
        assert_eq!(center.confirm(), Some(CenterAction::Capture));
        assert_eq!(center.capture_spec().unwrap().label, "model request id");
        assert_eq!(
            center.submit_capture("brand-new".into()),
            Some(CenterAction::Capture)
        );
        assert_eq!(center.capture_spec().unwrap().label, "display name");
        assert_eq!(
            center.submit_capture(String::new()),
            Some(CenterAction::AddCustomModel {
                name: "deepseek".into(),
                model: "brand-new".into(),
                display_name: "brand-new".into(),
            })
        );
        center.down();
        assert_eq!(
            center.confirm(),
            Some(CenterAction::DiscoverModels("deepseek".into()))
        );
    }

    #[test]
    fn advanced_edits_base_url_and_model_fields_one_at_a_time() {
        let mut center =
            ConfigurationCenter::new(vec![provider(ProviderStatus::Ready)], vec![kind()]);
        open_provider(&mut center);
        center.down();
        center.down();
        center.down();
        center.confirm();
        assert_eq!(center.page(), &CenterPage::Advanced("deepseek".into()));
        assert_eq!(center.rows()[0].description, "https://api.deepseek.com");
        assert_eq!(center.confirm(), Some(CenterAction::Capture));
        assert_eq!(center.capture_spec().unwrap().label, "base URL");
        assert_eq!(
            center.submit_capture("https://proxy.example.com/v1".into()),
            Some(CenterAction::SetProviderField {
                name: "deepseek".into(),
                field: ProviderFieldEdit::BaseUrl(Some("https://proxy.example.com/v1".into())),
            })
        );
        // Row 2 is the model entry.
        center.down();
        center.down();
        center.confirm();
        assert_eq!(
            center.page(),
            &CenterPage::ModelAdvanced {
                provider: "deepseek".into(),
                model: "deepseek-flash".into()
            }
        );
        center.down();
        center.confirm();
        assert_eq!(
            center.page(),
            &CenterPage::ModelChoice {
                provider: "deepseek".into(),
                model: "deepseek-flash".into(),
                kind: ModelChoiceKind::Transport,
            }
        );
        center.down();
        center.down();
        assert_eq!(
            center.confirm(),
            Some(CenterAction::SetModelField {
                name: "deepseek".into(),
                model: "deepseek-flash".into(),
                field: ModelFieldEdit::Transport(Some("responses".into())),
            })
        );
    }

    #[test]
    fn effort_map_editor_requires_every_exposed_effort() {
        let mut entry = provider(ProviderStatus::Ready).models.pop().unwrap();
        entry.efforts = vec![ReasoningEffort::Low, ReasoningEffort::High];
        entry.default_effort = ReasoningEffort::Low;
        let mut provider = provider(ProviderStatus::Ready);
        provider.models = vec![entry];
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        open_provider(&mut center);
        center.down();
        center.down();
        center.down();
        center.confirm(); // Advanced
        center.down();
        center.down();
        center.confirm(); // ModelAdvanced
        for _ in 0..9 {
            center.down();
        }
        center.confirm(); // Effort mapping
        assert_eq!(
            center.page(),
            &CenterPage::EffortMap {
                provider: "deepseek".into(),
                model: "deepseek-flash".into()
            }
        );
        // Selecting low -> form -> value -> capture -> low = "low".
        center.confirm();
        assert_eq!(
            center.page(),
            &CenterPage::EffortMapForm {
                provider: "deepseek".into(),
                model: "deepseek-flash".into(),
                effort: ReasoningEffort::Low
            }
        );
        center.down();
        assert_eq!(center.confirm(), Some(CenterAction::Capture));
        assert_eq!(center.capture_spec().unwrap().label, "low value");
        assert_eq!(center.submit_capture("low".into()), None);
        // High still has no form: save is blocked.
        center.down(); // highlight high
        center.confirm(); // open high form
        center.down();
        center.down();
        center.down();
        center.confirm(); // disabled
        assert_eq!(
            center.page(),
            &CenterPage::EffortMap {
                provider: "deepseek".into(),
                model: "deepseek-flash".into()
            }
        );
        center.down();
        center.down(); // save row
        let action = center.confirm().expect("complete mapping saves");
        let CenterAction::SetModelField {
            field: ModelFieldEdit::EffortMap(map),
            ..
        } = action
        else {
            panic!("expected effort map save");
        };
        assert_eq!(
            map[&ReasoningEffort::Low],
            EffortMapEdit::Value("low".into())
        );
        assert_eq!(map[&ReasoningEffort::High], EffortMapEdit::Disabled);
    }

    #[test]
    fn remove_requires_confirmation_and_back_keeps_provider() {
        let mut center =
            ConfigurationCenter::new(vec![provider(ProviderStatus::Ready)], vec![kind()]);
        open_provider(&mut center);
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
        let mut provider = provider(ProviderStatus::Ready);
        provider.available_models = vec!["deepseek-flash".into(), "deepseek-v4-pro".into()];
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        open_provider(&mut center);
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
    fn model_editor_keeps_default_enabled_and_shows_unresolved_entries() {
        let mut provider = provider(ProviderStatus::UnresolvedModels);
        provider.models.push(ProviderModelSummary {
            id: "unknown".into(),
            display_name: "Unknown".into(),
            enabled: false,
            resolved: false,
            ..ProviderModelSummary::default()
        });
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        open_provider(&mut center);
        center.down();
        center.confirm();
        assert_eq!(center.page(), &CenterPage::Models("deepseek".into()));
        assert!(center.rows()[1].description.contains("unresolved"));
        center.confirm(); // Default cannot be deselected.
        assert!(center.rows()[0].label.starts_with("[x]"));
        center.down();
        center.confirm(); // Unresolved cannot be enabled before Advanced resolves it.
        center.down();
        center.down();
        assert_eq!(
            center.confirm(),
            Some(CenterAction::SetEnabledModels {
                name: "deepseek".into(),
                models: vec!["deepseek-flash".into()],
            })
        );
    }

    #[test]
    fn discovery_adds_unknown_ids_as_unresolved_setup_rows() {
        let mut provider = provider(ProviderStatus::Ready);
        provider.kind = "opencode-zen".into();
        let mut center = ConfigurationCenter::new(vec![provider], vec![kind()]);
        center.merge_discovered("deepseek", &["deepseek-flash".into(), "unknown".into()]);
        assert_eq!(center.providers[0].models.len(), 2);
        assert!(center.providers[0].models[1].resolved == false);
        assert_eq!(center.providers[0].discovered_ids.len(), 2);
        assert!(
            !center.providers[0]
                .available_models
                .contains(&"unknown".into())
        );
    }

    #[test]
    fn explicit_new_session_default_emits_a_distinct_action() {
        let mut center =
            ConfigurationCenter::new(vec![provider(ProviderStatus::Ready)], vec![kind()]);
        open_provider(&mut center);
        for _ in 0..4 {
            center.down();
        }
        assert_eq!(
            center.confirm(),
            Some(CenterAction::SetNewSessionDefault("deepseek".into()))
        );
    }
}
