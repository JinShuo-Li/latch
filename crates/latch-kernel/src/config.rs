use crate::paths::ResolvedPaths;
use anyhow::{Context, Result};
use latch_protocol::{InputModality, Mode, ModelPricing, PermissionMode, ReasoningEffort, Safety};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Legacy single-provider table. Read for backward compatibility and
    /// migrated into [`Self::providers`] on save; never serialized again.
    #[serde(default, skip_serializing)]
    pub provider: ProviderConfig,
    /// User-defined provider instances, keyed by stable provider id.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderProfileConfig>,
    /// Default inference profile (provider, model, reasoning effort).
    #[serde(default)]
    pub inference: InferenceConfig,
    #[serde(default)]
    pub default_mode: Mode,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub permissions: PermissionConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub failure: FailureConfig,
    #[serde(default)]
    pub extensions: Vec<ExtensionConfig>,
    /// Central lifecycle deadlines for extension hosts, in whole seconds.
    /// Extensions are operator-installed but not trusted to answer forever:
    /// every stage that waits on one is bounded (see
    /// [`crate::extension::ExtensionLifecycle`]).
    #[serde(default)]
    pub extension_lifecycle: ExtensionLifecycleConfig,
    /// Legacy global per-model metadata. Still read as an explicit user
    /// override for any provider; folded into each provider entry on save and
    /// never serialized again.
    #[serde(default, skip_serializing)]
    pub models: BTreeMap<String, ModelConfig>,
}

/// Wire transport used for one model. Providers can expose several transports
/// at once (OpenCode Go serves GPT models over Responses, Claude/Qwen/MiniMax
/// over Messages, and the rest over Chat Completions), so transport is a model
/// capability, not a provider-wide constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// `POST {base}/chat/completions` (OpenAI-compatible chat completions).
    #[default]
    ChatCompletions,
    /// `POST {base}/responses` (OpenAI Responses API).
    Responses,
    /// `POST {base}/messages` (Anthropic Messages API).
    AnthropicMessages,
    /// `POST {base}/models/{model}:streamGenerateContent?alt=sse` (Google
    /// Generative Language API).
    Gemini,
}

impl TransportKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "messages",
            Self::Gemini => "gemini",
        }
    }

    /// Which effort mapping forms this transport can serialize. A form a
    /// transport cannot express is rejected at configuration validation, not
    /// silently dropped at request time.
    #[must_use]
    pub const fn supports_effort_forms(self) -> EffortForms {
        match self {
            // OpenAI-compatible chat completions carries `reasoning_effort`
            // and, on DeepSeek-family reasoning models, the `thinking` toggle.
            Self::ChatCompletions => EffortForms {
                value: true,
                budget_tokens: false,
                disabled: true,
            },
            // Responses carries `reasoning.effort`; there is no token budget or
            // separate off switch (the `none` level is the documented off).
            Self::Responses => EffortForms {
                value: true,
                budget_tokens: false,
                disabled: false,
            },
            // Messages carries `output_config.effort` for adaptive thinking and
            // `thinking.budget_tokens` / `thinking.type = "disabled"` for
            // classic thinking.
            Self::AnthropicMessages => EffortForms {
                value: true,
                budget_tokens: true,
                disabled: true,
            },
            // Gemini thinking forms are model-declared, not transport-wide:
            // Gemini 3 level models and Gemini 2.5 budget models accept
            // different controls, and some models cannot disable thinking at
            // all. Validation reads the resolved `gemini_thinking` capability
            // instead of assuming the broadest wire.
            Self::Gemini => EffortForms::NONE,
        }
    }
}

/// One exposed effort's wire form in a user `effort_map`. Exactly one of the
/// three documented forms is set; validation rejects empty, conflicting, and
/// transport-inexpressible mappings with an actionable message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EffortMapping {
    /// The transport's effort field value (`reasoning_effort`,
    /// `reasoning.effort`, `output_config.effort`,
    /// `thinkingConfig.thinkingLevel`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The transport's thinking token budget (`thinking.budget_tokens`,
    /// `thinkingConfig.thinkingBudget`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u64>,
    /// The transport's documented off switch (`thinking.type = "disabled"`,
    /// `thinkingConfig.thinkingBudget = 0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

/// The resolved single form of one [`EffortMapping`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffortForm {
    Value(String),
    BudgetTokens(u64),
    Disabled,
}

impl EffortForm {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Value(_) => "value",
            Self::BudgetTokens(_) => "budget_tokens",
            Self::Disabled => "disabled",
        }
    }
}

impl EffortMapping {
    /// Resolves the one documented form this mapping expresses. Unknown,
    /// empty, conflicting, and contradictory mappings are errors.
    pub fn form(&self) -> Result<EffortForm, String> {
        let forms = [
            self.value.is_some(),
            self.budget_tokens.is_some(),
            self.disabled.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if forms != 1 {
            return Err("must set exactly one of value, budget_tokens, or disabled".to_owned());
        }
        if let Some(value) = &self.value {
            if value.trim().is_empty() {
                return Err("value must not be empty".to_owned());
            }
            return Ok(EffortForm::Value(value.clone()));
        }
        if let Some(budget) = self.budget_tokens {
            if budget == 0 {
                return Err("budget_tokens must be positive".to_owned());
            }
            return Ok(EffortForm::BudgetTokens(budget));
        }
        match self.disabled {
            Some(true) => Ok(EffortForm::Disabled),
            Some(false) => Err("disabled must be true when present".to_owned()),
            None => unreachable!("form count checked above"),
        }
    }

    /// Whether this mapping expresses an explicit value form.
    #[must_use]
    pub fn is_value(&self) -> bool {
        matches!(self.form(), Ok(EffortForm::Value(_)))
    }
}

/// Which [`EffortMapping`] forms one wire transport can serialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffortForms {
    pub value: bool,
    pub budget_tokens: bool,
    pub disabled: bool,
}

impl EffortForms {
    /// No mapping form is expressible. Used when a model-level capability has
    /// not been declared yet.
    pub const NONE: Self = Self {
        value: false,
        budget_tokens: false,
        disabled: false,
    };

    #[must_use]
    pub const fn supports(self, form: &EffortForm) -> bool {
        match form {
            EffortForm::Value(_) => self.value,
            EffortForm::BudgetTokens(_) => self.budget_tokens,
            EffortForm::Disabled => self.disabled,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match (self.value, self.budget_tokens, self.disabled) {
            (true, true, true) => "value, budget_tokens, or disabled",
            (true, true, false) => "value or budget_tokens",
            (true, false, true) => "value or disabled",
            (true, false, false) => "value",
            (false, true, true) => "budget_tokens or disabled",
            (false, false, true) => "disabled",
            (false, true, false) => "budget_tokens",
            (false, false, false) => "no effort mapping",
        }
    }
}

/// One user-defined provider instance.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderProfileConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Symbolic credential reference: `env:NAME`, `file:NAME`, or `keyring:NAME`.
    /// Secret material is never stored here or in the durable event log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Selection only: absent means the complete built-in catalog plus custom
    /// entries. Model facts stay in the catalog or sparse `models` overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_models: Option<Vec<String>>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
    /// Reserved for optional remote model discovery. Never required at startup.
    #[serde(default)]
    pub model_discovery: bool,
}

/// Provider wire family. Adding a provider means adding an adapter variant, a
/// capability descriptor, and (optionally) built-in model metadata; it never
/// means adding conditionals to the TUI or the agent loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ProviderKind {
    #[default]
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "deepseek")]
    DeepSeek,
    #[serde(rename = "opencode-go")]
    OpenCodeGo,
    #[serde(rename = "opencode-zen")]
    OpenCodeZen,
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
}

impl ProviderKind {
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::DeepSeek => "deepseek",
            Self::OpenCodeGo => "opencode-go",
            Self::OpenCodeZen => "opencode-zen",
            Self::OpenAiCompatible => "openai-compatible",
        }
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::DeepSeek => "DeepSeek",
            Self::OpenCodeGo => "OpenCode Go",
            Self::OpenCodeZen => "OpenCode Zen",
            Self::OpenAiCompatible => "Custom OpenAI-compatible",
        }
    }

    #[must_use]
    pub const fn default_credential(self) -> &'static str {
        match self {
            Self::OpenAi => "env:OPENAI_API_KEY",
            Self::Anthropic => "env:ANTHROPIC_API_KEY",
            Self::DeepSeek => "env:DEEPSEEK_API_KEY",
            Self::OpenCodeGo => "env:OPENCODE_API_KEY",
            Self::OpenCodeZen => "env:OPENCODE_API_KEY",
            Self::OpenAiCompatible => "env:OPENAI_API_KEY",
        }
    }

    /// Parses legacy `provider.kind` strings and the canonical names. The
    /// legacy `openai-compatible` kind with an OpenCode Go base URL migrates to
    /// the explicit OpenCode Go profile.
    #[must_use]
    pub fn parse(raw: &str, base_url: Option<&str>) -> Option<Self> {
        let kind = match raw.trim().to_ascii_lowercase().as_str() {
            "openai" => Self::OpenAi,
            "anthropic" => Self::Anthropic,
            "deepseek" => Self::DeepSeek,
            "opencode-go" | "opencode_go" | "opencode" => Self::OpenCodeGo,
            "opencode-zen" | "opencode_zen" | "zen" => Self::OpenCodeZen,
            "openai-compatible" | "openai_compatible" | "generic" | "custom" => {
                if base_url.is_some_and(is_opencode_go_url) {
                    Self::OpenCodeGo
                } else {
                    Self::OpenAiCompatible
                }
            }
            _ => return None,
        };
        Some(kind)
    }

    #[must_use]
    pub const fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::DeepSeek => "https://api.deepseek.com",
            Self::OpenCodeGo => "https://opencode.ai/zen/go/v1",
            Self::OpenCodeZen => "https://opencode.ai/zen/v1",
            Self::OpenAiCompatible => "",
        }
    }

    /// Default wire transport for models on this provider kind. Current OpenAI
    /// reasoning models require the Responses API for tool calling with
    /// reasoning effort; other families keep their native transport.
    #[must_use]
    pub const fn default_transport(self) -> TransportKind {
        match self {
            Self::OpenAi => TransportKind::Responses,
            Self::Anthropic => TransportKind::AnthropicMessages,
            Self::DeepSeek | Self::OpenCodeGo | Self::OpenCodeZen | Self::OpenAiCompatible => {
                TransportKind::ChatCompletions
            }
        }
    }

    /// Whether the kind's declared default transport resolves models that are
    /// not in a built-in catalog. True only for a user-declared
    /// OpenAI-compatible provider, whose protocol is chosen when the provider
    /// is added; catalog providers never invent a transport for unknown ids.
    #[must_use]
    pub const fn resolves_unknown_models(self) -> bool {
        matches!(self, Self::OpenAiCompatible)
    }
}

#[must_use]
pub fn is_opencode_go_url(base_url: &str) -> bool {
    match base_url
        .trim_end_matches('/')
        .strip_prefix("https://opencode.ai/zen/go")
    {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// Default inference profile selection in `[inference]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InferenceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: ReasoningEffort,
}

/// Efforts a model exposes: an explicit user list, the built-in catalog set, or
/// none for a fully custom model. `ProviderDefault` is a selection state, never
/// an exposed level.
fn exposed_efforts(
    kind: ProviderKind,
    model: &str,
    override_: &ModelConfig,
) -> Vec<ReasoningEffort> {
    if let Some(efforts) = &override_.efforts {
        return efforts
            .iter()
            .copied()
            .filter(|effort| !matches!(effort, ReasoningEffort::ProviderDefault))
            .collect();
    }
    crate::providers::builtin_catalog(kind)
        .iter()
        .find(|descriptor| descriptor.model == model)
        .map(|descriptor| descriptor.supported_efforts.clone())
        .unwrap_or_default()
}

/// The transport a model override will actually serialize through: explicit
/// override, built-in catalog, then the provider-kind default.
fn resolved_transport(kind: ProviderKind, model: &str, override_: &ModelConfig) -> TransportKind {
    override_.transport.unwrap_or_else(|| {
        crate::providers::builtin_catalog(kind)
            .iter()
            .find(|descriptor| descriptor.model == model)
            .map(|descriptor| descriptor.transport)
            .unwrap_or_else(|| kind.default_transport())
    })
}

/// Whether the model uses Anthropic adaptive thinking rather than classic
/// thinking with an explicit budget. This decides which effort mapping forms
/// the Messages transport can honestly represent.
fn resolved_adaptive(kind: ProviderKind, model: &str, override_: &ModelConfig) -> bool {
    override_.adaptive_thinking.unwrap_or_else(|| {
        crate::providers::builtin_catalog(kind)
            .iter()
            .find(|descriptor| descriptor.model == model)
            .is_some_and(|descriptor| descriptor.adaptive_thinking)
    })
}

/// The Gemini thinking capability that applies to one model: an explicit user
/// declaration first, then the built-in catalog row. Only a Gemini transport
/// consults it; an undeclared model stays conservative.
fn resolved_gemini_thinking(
    kind: ProviderKind,
    model: &str,
    override_: &ModelConfig,
) -> Option<crate::provider::GeminiThinkingCapability> {
    if resolved_transport(kind, model, override_) != TransportKind::Gemini {
        return None;
    }
    override_.gemini_thinking.clone().or_else(|| {
        crate::providers::builtin_catalog(kind)
            .iter()
            .find(|descriptor| descriptor.model == model)
            .and_then(|descriptor| descriptor.gemini_thinking.clone())
    })
}

/// Validates exposed efforts, the default effort, and any explicit wire map.
/// A present map must cover every exposed level; a mapping a transport cannot
/// express is rejected here rather than silently dropped at request time.
fn validate_effort_metadata(
    provider: &str,
    kind: ProviderKind,
    model: &str,
    override_: &ModelConfig,
) -> Result<()> {
    let exposed = exposed_efforts(kind, model, override_);
    if let Some(default) = override_.default_effort
        && default != ReasoningEffort::ProviderDefault
        && !exposed.contains(&default)
    {
        anyhow::bail!(
            "provider {provider} model {model:?} default_effort {} is not in the exposed efforts {:?}",
            default.label(),
            exposed
        );
    }
    let transport = resolved_transport(kind, model, override_);
    let gemini = (transport == TransportKind::Gemini)
        .then(|| resolved_gemini_thinking(kind, model, override_))
        .flatten();
    if override_.gemini_thinking.is_some() && transport != TransportKind::Gemini {
        anyhow::bail!(
            "provider {provider} model {model:?} gemini_thinking is only valid with the gemini transport"
        );
    }
    if let Some(capability) = &gemini {
        capability.validate().map_err(|error| {
            anyhow::anyhow!("provider {provider} model {model:?} gemini_thinking {error}")
        })?;
    }
    if override_.effort_map.is_empty() {
        // No map: the adapter's neutral mapping applies. A level the declared
        // capability cannot express emits no control (the model's own default
        // applies), exactly like `provider default`.
        return Ok(());
    }
    let forms = match transport {
        TransportKind::Gemini => gemini.as_ref().map_or(
            EffortForms::NONE,
            crate::provider::GeminiThinkingCapability::effort_forms,
        ),
        _ => transport.supports_effort_forms(),
    };
    let adaptive = resolved_adaptive(kind, model, override_);
    for (effort, mapping) in &override_.effort_map {
        if matches!(effort, ReasoningEffort::ProviderDefault) {
            anyhow::bail!(
                "provider {provider} model {model:?} effort_map cannot map the provider default; map exposed levels only"
            );
        }
        if !exposed.contains(effort) {
            anyhow::bail!(
                "provider {provider} model {model:?} effort_map maps {} but the model does not expose that effort",
                effort.label()
            );
        }
        let form = mapping.form().map_err(|error| {
            anyhow::anyhow!(
                "provider {provider} model {model:?} effort_map {} {error}",
                effort.label()
            )
        })?;
        if !forms.supports(&form) {
            if transport == TransportKind::Gemini && gemini.is_none() {
                anyhow::bail!(
                    "provider {provider} model {model:?} effort_map {} uses {} but the gemini transport has no declared thinking capability for the model; set gemini_thinking first",
                    effort.label(),
                    form.label()
                );
            }
            anyhow::bail!(
                "provider {provider} model {model:?} effort_map {} uses {} but the {} transport only supports {}",
                effort.label(),
                form.label(),
                transport.label(),
                forms.label()
            );
        }
        if transport == TransportKind::Gemini
            && let Some(capability) = &gemini
            && let EffortForm::Value(value) = &form
            && !capability.supports_level(value)
        {
            anyhow::bail!(
                "provider {provider} model {model:?} effort_map {} value {value:?} is not one of the declared gemini thinking levels",
                effort.label()
            );
        }
        match (&form, transport) {
            (EffortForm::Value(_), TransportKind::AnthropicMessages) if !adaptive => {
                anyhow::bail!(
                    "provider {provider} model {model:?} effort_map {} uses the value form, which the messages transport only supports with adaptive_thinking; use budget_tokens",
                    effort.label()
                );
            }
            (EffortForm::BudgetTokens(_), TransportKind::AnthropicMessages) if adaptive => {
                anyhow::bail!(
                    "provider {provider} model {model:?} effort_map {} uses budget_tokens, which the messages transport only supports for classic thinking; adaptive models use the value form",
                    effort.label()
                );
            }
            _ => {}
        }
    }
    for effort in &exposed {
        if !override_.effort_map.contains_key(effort) {
            anyhow::bail!(
                "provider {provider} model {model:?} effort_map must cover the exposed effort {}",
                effort.label()
            );
        }
    }
    Ok(())
}

/// Product metadata for one model name, keyed by the provider model string.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
    /// Provider context window in tokens. `None` uses the conservative
    /// [`DEFAULT_CONTEXT_WINDOW_TOKENS`] fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<usize>,
    /// Explicit supported reasoning efforts. `Some(vec![])` means the model
    /// exposes no configurable effort; `None` means "use the built-in
    /// capability or the conservative default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Vec<ReasoningEffort>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<ReasoningEffort>,
    /// Per-level wire form for Custom/Advanced models. Empty means the
    /// transport adapter uses its built-in mapping, which is the normal path
    /// for every catalog model. A present map must cover every exposed level.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effort_map: BTreeMap<ReasoningEffort, EffortMapping>,
    /// Declared Gemini thinking wire capability for Custom/Advanced models.
    /// `None` uses the built-in catalog capability, or stays conservative (no
    /// thinking controls) when neither declares one. Gemini only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini_thinking: Option<crate::provider::GeminiThinkingCapability>,
    /// Whether persisted assistant reasoning must be replayed to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_replay: Option<ReasoningReplayPolicy>,
    /// Wire transport override. `None` uses the built-in model capability or
    /// the provider-kind default. Useful for proxies that emulate one API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportKind>,
    /// Anthropic adaptive thinking override. `None` uses the built-in model
    /// capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    /// Provider-neutral input modalities, for example `["text", "image"]`.
    /// `None` uses the built-in model capability; `Some` overrides it, so a
    /// custom or newly released vision model never requires a Latch rebuild.
    /// Text is always implicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<Vec<InputModality>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// Whether the provider requires persisted `reasoning_content` to be replayed
/// verbatim. Kept provider-neutral at the configuration layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningReplayPolicy {
    Replay,
    Omit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_provider")]
    pub kind: String,
    #[serde(default = "default_model")]
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

/// Safety profile defaults. `[safety] level = "strict" | "standard" | "autonomous"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    #[serde(default)]
    pub level: Safety,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            level: Safety::Standard,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionConfig {
    #[serde(default = "yes")]
    pub workspace_write: bool,
    #[serde(default)]
    pub outside_workspace: OutsidePolicy,
    #[serde(default = "default_shell_timeout")]
    pub shell_timeout_seconds: u64,
    /// Permission resolver: `auto_approve`, `human`, or `ai_review`.
    #[serde(default)]
    pub mode: PermissionMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutsidePolicy {
    /// Outside-workspace writes are classified but rejected outright. The
    /// default is `Ask`: external effects always become a kernel Ask first.
    Deny,
    #[default]
    Ask,
}

/// Token-native context budget. Bytes remain only for internal file, artifact,
/// log, and I/O limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Explicit cap on the estimated complete request, overriding the derived
    /// context window minus reserve. `None` uses the model window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_tokens: Option<usize>,
    /// Upper bound for the verbatim recent transcript within the request.
    #[serde(default = "default_recent_tokens")]
    pub recent_tokens: usize,
    /// Tokens reserved for the model's own response.
    #[serde(default = "default_output_reserve_tokens")]
    pub output_reserve_tokens: usize,
    /// Additional safety reserve held back from the request budget.
    #[serde(default = "default_safety_reserve_tokens")]
    pub reserve_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureConfig {
    #[serde(default = "default_retry")]
    pub retry_budget: u32,
    /// Consecutive model turns that only repeat unchanged inspections are
    /// tolerated before the kernel re-grounds the model.
    #[serde(default = "default_stagnation")]
    pub stagnation_budget: u32,
    /// Ultimate abnormal-behaviour circuit breaker. `None` (the default) means
    /// long, productive tasks are never killed by turn count; the stagnation
    /// and failure supervisors remain the primary loop controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_turns: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// Configurable extension lifecycle deadlines, in whole seconds. Defaults are
/// the single central policy in [`crate::extension::ExtensionLifecycle`];
/// values must be positive (a zero-second deadline expires immediately).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionLifecycleConfig {
    /// Process spawn plus the `initialize` request write deadline.
    #[serde(
        default = "default_extension_spawn_seconds",
        deserialize_with = "positive_seconds"
    )]
    pub spawn_seconds: u64,
    /// `initialize` response deadline.
    #[serde(
        default = "default_extension_initialize_seconds",
        deserialize_with = "positive_seconds"
    )]
    pub initialize_seconds: u64,
    /// Registration collection deadline, until the `ready` notification.
    #[serde(
        default = "default_extension_ready_seconds",
        deserialize_with = "positive_seconds"
    )]
    pub ready_seconds: u64,
    /// One ordinary extension RPC deadline.
    #[serde(
        default = "default_extension_request_seconds",
        deserialize_with = "positive_seconds"
    )]
    pub request_seconds: u64,
    /// `shutdown` response deadline.
    #[serde(
        default = "default_extension_shutdown_seconds",
        deserialize_with = "positive_seconds"
    )]
    pub shutdown_seconds: u64,
    /// Graceful child exit deadline before the child is killed and reaped.
    #[serde(
        default = "default_extension_exit_seconds",
        deserialize_with = "positive_seconds"
    )]
    pub exit_seconds: u64,
}

fn positive_seconds<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "extension_lifecycle timeout must be a positive number of seconds",
        ));
    }
    Ok(value)
}

impl Default for ExtensionLifecycleConfig {
    fn default() -> Self {
        Self {
            spawn_seconds: default_extension_spawn_seconds(),
            initialize_seconds: default_extension_initialize_seconds(),
            ready_seconds: default_extension_ready_seconds(),
            request_seconds: default_extension_request_seconds(),
            shutdown_seconds: default_extension_shutdown_seconds(),
            exit_seconds: default_extension_exit_seconds(),
        }
    }
}

fn default_extension_spawn_seconds() -> u64 {
    crate::extension::DEFAULT_SPAWN_SECONDS
}
fn default_extension_initialize_seconds() -> u64 {
    crate::extension::DEFAULT_INITIALIZE_SECONDS
}
fn default_extension_ready_seconds() -> u64 {
    crate::extension::DEFAULT_READY_SECONDS
}
fn default_extension_request_seconds() -> u64 {
    crate::extension::DEFAULT_REQUEST_SECONDS
}
fn default_extension_shutdown_seconds() -> u64 {
    crate::extension::DEFAULT_SHUTDOWN_SECONDS
}
fn default_extension_exit_seconds() -> u64 {
    crate::extension::DEFAULT_EXIT_SECONDS
}

fn default_provider() -> String {
    "openai".into()
}
fn default_model() -> String {
    "gpt-5-mini".into()
}
/// Fallback context window when a model has no configured metadata. Users can
/// override it per model with `[models.<name>] context_window_tokens = ...`.
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 256_000;
fn default_recent_tokens() -> usize {
    24_000
}
fn default_output_reserve_tokens() -> usize {
    8_192
}
fn default_safety_reserve_tokens() -> usize {
    4_000
}
fn default_retry() -> u32 {
    3
}
fn default_stagnation() -> u32 {
    crate::progress::DEFAULT_STAGNATION_BUDGET
}
fn default_shell_timeout() -> u64 {
    120
}
const fn yes() -> bool {
    true
}

fn default_state_dir() -> PathBuf {
    ResolvedPaths::resolve(None, None).state_root
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: default_provider(),
            model: default_model(),
            base_url: None,
            api_key_env: None,
        }
    }
}
impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            workspace_write: true,
            outside_workspace: OutsidePolicy::Ask,
            shell_timeout_seconds: default_shell_timeout(),
            mode: PermissionMode::Human,
        }
    }
}
impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_request_tokens: None,
            recent_tokens: default_recent_tokens(),
            output_reserve_tokens: default_output_reserve_tokens(),
            reserve_tokens: default_safety_reserve_tokens(),
        }
    }
}
impl Default for FailureConfig {
    fn default() -> Self {
        Self {
            retry_budget: default_retry(),
            stagnation_budget: default_stagnation(),
            max_model_turns: None,
        }
    }
}
impl Default for Config {
    fn default() -> Self {
        Self {
            provider: ProviderConfig::default(),
            providers: BTreeMap::new(),
            inference: InferenceConfig::default(),
            default_mode: Mode::Work,
            state_dir: default_state_dir(),
            safety: SafetyConfig::default(),
            permissions: PermissionConfig::default(),
            context: ContextConfig::default(),
            failure: FailureConfig::default(),
            extensions: vec![],
            extension_lifecycle: ExtensionLifecycleConfig::default(),
            models: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Validate user-owned config fields before a new setup transaction is
    /// staged. Provider adapters still enforce their own wire constraints.
    pub fn validate(&self) -> Result<()> {
        for (id, entry) in &self.providers {
            if id.is_empty()
                || !id.chars().all(|ch| {
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-')
                })
            {
                anyhow::bail!("provider id {id:?} must use [a-z0-9._-]");
            }
            if let Some(url) = &entry.base_url {
                let parsed = reqwest::Url::parse(url)
                    .with_context(|| format!("provider {id} base_url must be a valid URL"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    anyhow::bail!("provider {id} base_url must use http or https");
                }
            }
            if let Some(reference) = &entry.credential {
                reference
                    .parse::<crate::credentials::CredentialRef>()
                    .map_err(|error| anyhow::anyhow!("provider {id} credential: {error}"))?;
            }
            if entry.default_model.as_deref().is_none_or(str::is_empty) {
                anyhow::bail!("provider {id} needs a default_model");
            }
            for (model, override_) in &entry.models {
                if model.trim().is_empty() {
                    anyhow::bail!("provider {id} has an empty model request id");
                }
                if override_.context_window_tokens == Some(0) {
                    anyhow::bail!("provider {id} model {model:?} needs a positive context window");
                }
                if let Some(pricing) = &override_.pricing {
                    for (field, value) in [
                        ("input_per_million", pricing.input_per_million),
                        ("output_per_million", pricing.output_per_million),
                        ("cache_read_per_million", pricing.cache_read_per_million),
                        ("cache_write_per_million", pricing.cache_write_per_million),
                    ] {
                        if let Some(value) = value
                            && (!value.is_finite() || value < 0.0)
                        {
                            anyhow::bail!(
                                "provider {id} model {model:?} pricing {field} must be a non-negative number"
                            );
                        }
                    }
                    if pricing.currency.trim().is_empty() {
                        anyhow::bail!(
                            "provider {id} model {model:?} pricing currency must not be empty"
                        );
                    }
                }
                validate_effort_metadata(id, entry.kind, model, override_)?;
            }
        }
        let registry = crate::providers::ProviderRegistry::from_config(self)?;
        registry.default_profile(self)?;
        Ok(())
    }

    /// Resolved optional pricing for an exact provider model name.
    #[must_use]
    pub fn pricing_for(&self, model: &str) -> Option<&ModelPricing> {
        self.models
            .get(model)
            .and_then(|config| config.pricing.as_ref())
    }

    /// Context window for an exact provider model name, falling back to the
    /// conservative default.
    #[must_use]
    pub fn context_window_for(&self, model: &str) -> usize {
        self.models
            .get(model)
            .and_then(|config| config.context_window_tokens)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS)
    }

    /// Tokens set aside for the model response plus safety.
    #[must_use]
    pub fn reserve_tokens(&self) -> usize {
        self.context
            .output_reserve_tokens
            .saturating_add(self.context.reserve_tokens)
    }

    /// Token budget for the complete request sent to the model.
    #[must_use]
    pub fn request_budget_for(&self, model: &str) -> usize {
        let window = self.context_window_for(model);
        self.context
            .max_request_tokens
            .unwrap_or_else(|| window.saturating_sub(self.reserve_tokens()))
            .min(window)
    }

    /// The config file path `Config::load` would use.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        Some(ResolvedPaths::resolve(None, None).config_path)
    }

    pub fn load(path: Option<&Path>) -> Result<Self> {
        let paths = ResolvedPaths::resolve(path, None);
        let path = paths.config_path;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let has_state_override = text
            .parse::<toml::Table>()
            .with_context(|| format!("parse {}", path.display()))?
            .contains_key("state_dir");
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        if !has_state_override {
            config.state_dir = paths.state_root;
        }
        Ok(config)
    }

    /// Writes the canonical configuration representation atomically. Existing
    /// legacy keys are superseded by the normalized `[providers.*]` tables; no
    /// secret material is ever written here.
    /// Migrates legacy configuration into the canonical multi-provider shape.
    ///
    /// - `[provider]` becomes a `[providers.<kind>]` entry (named by kind) and
    ///   seeds `[inference]` when it is unset.
    /// - global `[models.*]` metadata is folded into every configured provider
    ///   without overriding provider-specific entries.
    ///
    /// Semantics are preserved; formatting and comments are not (see the
    /// canonical-write note in the docs).
    pub fn normalize(&mut self) {
        let mut legacy_created = false;
        if self.providers.is_empty()
            && let Some(kind) =
                ProviderKind::parse(&self.provider.kind, self.provider.base_url.as_deref())
        {
            let credential = self
                .provider
                .api_key_env
                .clone()
                .map(|env| format!("env:{env}"))
                .unwrap_or_else(|| kind.default_credential().to_owned());
            self.providers.insert(
                kind.id().to_owned(),
                ProviderProfileConfig {
                    kind,
                    display_name: None,
                    base_url: self.provider.base_url.clone(),
                    credential: Some(credential),
                    default_model: Some(self.provider.model.clone()),
                    enabled_models: None,
                    models: BTreeMap::new(),
                    model_discovery: false,
                },
            );
            legacy_created = true;
            if self.inference.provider.is_none() {
                self.inference.provider = Some(kind.id().to_owned());
            }
            if self.inference.model.is_none() {
                self.inference.model = Some(self.provider.model.clone());
            }
        }
        if !self.models.is_empty() {
            let legacy = std::mem::take(&mut self.models);
            for entry in self.providers.values_mut() {
                for (name, meta) in &legacy {
                    entry
                        .models
                        .entry(name.clone())
                        .or_insert_with(|| meta.clone());
                }
            }
        }
        if legacy_created
            && let Some(entry) = self.providers.values_mut().next()
            && let Some(model) = &entry.default_model
            && !crate::providers::builtin_catalog(entry.kind)
                .iter()
                .any(|known| known.model == *model)
        {
            let override_ = entry.models.entry(model.clone()).or_default();
            override_
                .transport
                .get_or_insert(entry.kind.default_transport());
        }
        self.provider = ProviderConfig::default();
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let staged = self.stage(path)?;
        Self::commit_stage(staged, path)
    }

    /// Stage a complete config in its target directory. The returned file is
    /// removed automatically if validation or a later stage fails.
    pub fn stage(&self, path: &Path) -> Result<tempfile::NamedTempFile> {
        let mut canonical = self.clone();
        canonical.normalize();
        let text = toml::to_string_pretty(&canonical).context("serialize config")?;
        if let Some(parent) = path.parent() {
            // Only the default storage root is owned by Latch. An explicit
            // --config may live in an operator-managed directory.
            if parent == ResolvedPaths::resolve(None, None).state_root {
                ResolvedPaths::ensure_private_root(parent)
            } else {
                std::fs::create_dir_all(parent)
            }
            .with_context(|| format!("create {}", parent.display()))?;
        }
        use std::io::Write;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("stage config in {}", parent.display()))?;
        temp.write_all(text.as_bytes())
            .context("write staged config")?;
        temp.as_file().sync_all().context("sync staged config")?;
        Ok(temp)
    }

    pub fn commit_stage(staged: tempfile::NamedTempFile, path: &Path) -> Result<()> {
        staged
            .persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("replace {}", path.display()))?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_configuration_parses() {
        let config: Config = toml::from_str(include_str!("../../../config.example.toml")).unwrap();
        assert_eq!(config.default_mode, Mode::Work);
        assert_eq!(config.provider.kind, "openai-compatible");
        assert_eq!(config.context.recent_tokens, 24_000);
        assert_eq!(
            config.context_window_for("gpt-5-mini"),
            DEFAULT_CONTEXT_WINDOW_TOKENS
        );
        assert_eq!(
            config.request_budget_for("gpt-5-mini"),
            DEFAULT_CONTEXT_WINDOW_TOKENS - config.reserve_tokens()
        );
        assert!(config.models.is_empty(), "pricing is optional");
    }

    #[test]
    fn setup_validation_rejects_bad_ids_urls_and_model_overrides() {
        let base = "[providers.good]\nkind = 'openai'\ndefault_model = 'gpt-5.5'\n";
        for (input, expected) in [
            (
                "[providers.'Bad ID']\nkind = 'openai'\ndefault_model = 'gpt-5.5'\n",
                "provider id",
            ),
            (
                "[providers.good]\nkind = 'openai'\nbase_url = 'file:///tmp/model'\ndefault_model = 'gpt-5.5'\n",
                "http or https",
            ),
            (
                "[providers.good]\nkind = 'openai'\ncredential = 'env:NOT-VALID'\ndefault_model = 'gpt-5.5'\n",
                "identifier",
            ),
            ("[providers.good]\nkind = 'openai'\n", "default_model"),
            (
                "[providers.good]\nkind = 'openai'\ndefault_model = 'gpt-5.5'\n[providers.good.models.'gpt-5.5']\ncontext_window_tokens = 0\n",
                "positive context",
            ),
            (
                "[providers.good]\nkind = 'openai'\ndefault_model = 'gpt-5.5'\n[providers.good.models.'gpt-5.5'.pricing]\ninput_per_million = -1.0\n",
                "non-negative number",
            ),
            (
                "[providers.good]\nkind = 'openai'\ndefault_model = 'gpt-5.5'\n[providers.good.models.'gpt-5.5'.pricing]\ncurrency = ''\n",
                "currency must not be empty",
            ),
        ] {
            let config: Config = toml::from_str(input).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(expected), "{input}: {error}");
        }
        let config: Config = toml::from_str(base).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn effort_map_forms_parse_serialize_and_validate() {
        let config: Config = toml::from_str(
            r#"
            [providers.classic]
            kind = "anthropic"
            default_model = "classic-pro"

            [providers.classic.models."classic-pro"]
            transport = "anthropic_messages"
            adaptive_thinking = false
            efforts = ["low", "high"]
            default_effort = "low"

            [providers.classic.models."classic-pro".effort_map]
            low = { budget_tokens = 1024 }
            high = { budget_tokens = 32768 }

            [providers.adaptive]
            kind = "anthropic"
            default_model = "adaptive-pro"

            [providers.adaptive.models."adaptive-pro"]
            transport = "anthropic_messages"
            adaptive_thinking = true
            efforts = ["low", "high"]
            default_effort = "low"
            [providers.adaptive.models."adaptive-pro".effort_map]
            low = { value = "low" }
            high = { value = "high" }

            [providers.toggle]
            kind = "openai-compatible"
            base_url = "https://example.com/v1"
            default_model = "toggle-pro"

            [providers.toggle.models."toggle-pro"]
            transport = "chat_completions"
            efforts = ["low", "high"]

            [providers.toggle.models."toggle-pro".effort_map]
            low = { disabled = true }
            high = { value = "high" }
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let classic = &config.providers["classic"].models["classic-pro"].effort_map;
        assert_eq!(
            classic[&ReasoningEffort::High].form().unwrap(),
            EffortForm::BudgetTokens(32_768)
        );
        let adaptive = &config.providers["adaptive"].models["adaptive-pro"].effort_map;
        assert_eq!(
            adaptive[&ReasoningEffort::Low].form().unwrap(),
            EffortForm::Value("low".into())
        );
        let toggle = &config.providers["toggle"].models["toggle-pro"].effort_map;
        assert_eq!(
            toggle[&ReasoningEffort::Low].form().unwrap(),
            EffortForm::Disabled
        );
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(
            text.contains("effort_map.low]") && text.contains("budget_tokens = 1024"),
            "{text}"
        );
        assert!(text.contains("disabled = true"), "{text}");
        let reloaded: Config = toml::from_str(&text).unwrap();
        reloaded.validate().unwrap();
        assert_eq!(
            &reloaded.providers["toggle"].models["toggle-pro"].effort_map,
            toggle
        );
    }

    #[test]
    fn effort_map_rejects_incomplete_malformed_and_contradictory_mappings() {
        for (body, expected) in [
            (
                "efforts = ['low', 'high']\n[providers.acme.models.m.effort_map]\nlow = { value = 'a' }\n",
                "must cover the exposed effort high",
            ),
            (
                "efforts = ['low']\n[providers.acme.models.m.effort_map]\nlow = { value = '' }\n",
                "value must not be empty",
            ),
            (
                "efforts = ['low']\n[providers.acme.models.m.effort_map]\nlow = { budget_tokens = 0 }\n",
                "budget_tokens must be positive",
            ),
            (
                "efforts = ['low']\n[providers.acme.models.m.effort_map]\nlow = { disabled = false }\n",
                "disabled must be true",
            ),
            (
                "efforts = ['low']\n[providers.acme.models.m.effort_map]\nlow = { value = 'a', disabled = true }\n",
                "exactly one of value, budget_tokens, or disabled",
            ),
            (
                "efforts = ['low']\n[providers.acme.models.m.effort_map]\nlow = {}\n",
                "exactly one of value, budget_tokens, or disabled",
            ),
            (
                "efforts = ['low']\n[providers.acme.models.m.effort_map]\nprovider_default = { value = 'a' }\nlow = { value = 'a' }\n",
                "cannot map the provider default",
            ),
            (
                "efforts = ['low']\n[providers.acme.models.m.effort_map]\nhigh = { value = 'a' }\nlow = { value = 'a' }\n",
                "does not expose that effort",
            ),
            (
                "efforts = ['low']\ndefault_effort = 'high'\n",
                "is not in the exposed efforts",
            ),
        ] {
            let input = format!(
                "[providers.acme]\nkind = 'openai-compatible'\ndefault_model = 'm'\n[providers.acme.models.m]\n{body}"
            );
            let config: Config = toml::from_str(&input).unwrap_or_else(|error| {
                panic!("{input}: {error}");
            });
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(expected), "{input}: {error}");
        }
    }

    #[test]
    fn effort_map_is_rejected_for_transports_without_the_form() {
        // Responses has effort values but no token budget and no off switch.
        let config: Config = toml::from_str(
            r#"
            [providers.openai]
            kind = "openai"
            default_model = "gpt-5.5"
            [providers.openai.models."gpt-5.5"]
            efforts = ["low", "high"]
            default_effort = "low"
            [providers.openai.models."gpt-5.5".effort_map]
            low = { budget_tokens = 1024 }
            high = { value = "high" }
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("responses transport only supports value"),
            "{error}"
        );

        // Chat completions cannot express a thinking budget.
        let config: Config = toml::from_str(
            r#"
            [providers.acme]
            kind = "openai-compatible"
            default_model = "m"
            [providers.acme.models.m]
            efforts = ["low"]
            [providers.acme.models.m.effort_map]
            low = { budget_tokens = 1024 }
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("chat completions transport only supports value or disabled"),
            "{error}"
        );

        // Messages resolves the form against the model's thinking mode.
        let adaptive: Config = toml::from_str(
            r#"
            [providers.anthropic]
            kind = "anthropic"
            default_model = "claude-sonnet-5"
            [providers.anthropic.models."claude-sonnet-5"]
            efforts = ["low", "high"]
            [providers.anthropic.models."claude-sonnet-5".effort_map]
            low = { budget_tokens = 2048 }
            high = { value = "high" }
            "#,
        )
        .unwrap();
        let error = adaptive.validate().unwrap_err().to_string();
        assert!(
            error.contains("only supports for classic thinking"),
            "{error}"
        );

        let classic: Config = toml::from_str(
            r#"
            [providers.anthropic]
            kind = "anthropic"
            default_model = "claude-haiku-4-5"
            [providers.anthropic.models."claude-haiku-4-5"]
            adaptive_thinking = false
            efforts = ["low", "high"]
            [providers.anthropic.models."claude-haiku-4-5".effort_map]
            low = { value = "low" }
            high = { value = "high" }
            "#,
        )
        .unwrap();
        let error = classic.validate().unwrap_err().to_string();
        assert!(
            error.contains("only supports with adaptive_thinking"),
            "{error}"
        );
    }

    #[test]
    fn gemini_effort_mapping_resolves_against_the_model_capability() {
        // Level-only builtin model: a token budget is not expressible.
        let config: Config = toml::from_str(
            r#"
            [providers.zen]
            kind = "opencode-zen"
            default_model = "gemini-3.8-flash"
            [providers.zen.models."gemini-3.8-flash"]
            efforts = ["low", "high"]
            [providers.zen.models."gemini-3.8-flash".effort_map]
            low = { value = "low" }
            high = { budget_tokens = 8192 }
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("gemini transport only supports value"),
            "{error}"
        );

        // A model that cannot disable thinking rejects the off form.
        let config: Config = toml::from_str(
            r#"
            [providers.zen]
            kind = "opencode-zen"
            default_model = "gemini-3.1-pro"
            [providers.zen.models."gemini-3.1-pro"]
            efforts = ["low", "high"]
            [providers.zen.models."gemini-3.1-pro".effort_map]
            low = { value = "low" }
            high = { disabled = true }
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("gemini transport only supports value"),
            "{error}"
        );

        // A mapped value must be one of the model's declared levels.
        let config: Config = toml::from_str(
            r#"
            [providers.zen]
            kind = "opencode-zen"
            default_model = "gemini-3.8-flash"
            [providers.zen.models."gemini-3.8-flash"]
            efforts = ["low", "high"]
            [providers.zen.models."gemini-3.8-flash".effort_map]
            low = { value = "minimal" }
            high = { value = "high" }
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("is not one of the declared gemini thinking levels"),
            "{error}"
        );

        // An exposed level the capability cannot express without a map keeps
        // validating: without a map the adapter emits no control and the
        // model's own default applies, exactly like `provider default`.
        let config: Config = toml::from_str(
            r#"
            [providers.zen]
            kind = "opencode-zen"
            default_model = "gemini-3.8-flash"
            [providers.zen.models."gemini-3.8-flash"]
            efforts = ["minimal", "high"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn custom_gemini_capability_is_explicit_and_validated() {
        // A budget model that allows zero maps `none` to the documented off
        // switch; the custom provider declares the capability itself.
        let config: Config = toml::from_str(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://gemini.example.com/v1beta"
            default_model = "gemini-2.5-flash"
            [providers.custom.models."gemini-2.5-flash"]
            transport = "gemini"
            efforts = ["none", "low"]
            default_effort = "none"
            [providers.custom.models."gemini-2.5-flash".gemini_thinking]
            mode = "budget"
            zero_allowed = true
            [providers.custom.models."gemini-2.5-flash".effort_map]
            none = { disabled = true }
            low = { budget_tokens = 4096 }
            "#,
        )
        .unwrap();
        config.validate().unwrap();

        // The same shape without the documented zero switch rejects `disabled`.
        let config: Config = toml::from_str(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://gemini.example.com/v1beta"
            default_model = "gemini-2.5-pro"
            [providers.custom.models."gemini-2.5-pro"]
            transport = "gemini"
            efforts = ["none", "low"]
            default_effort = "none"
            [providers.custom.models."gemini-2.5-pro".gemini_thinking]
            mode = "budget"
            zero_allowed = false
            [providers.custom.models."gemini-2.5-pro".effort_map]
            none = { disabled = true }
            low = { budget_tokens = 4096 }
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("gemini transport only supports budget_tokens"),
            "{error}"
        );

        // Without a declared capability no thinking form is expressible.
        let config: Config = toml::from_str(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://gemini.example.com/v1beta"
            default_model = "gemini-x"
            [providers.custom.models."gemini-x"]
            transport = "gemini"
            efforts = ["low"]
            [providers.custom.models."gemini-x".effort_map]
            low = { value = "low" }
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("has no declared thinking capability"),
            "{error}"
        );

        // Exposing an effort without a map is allowed: the adapter omits the
        // control and the model's own default applies.
        let config: Config = toml::from_str(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://gemini.example.com/v1beta"
            default_model = "gemini-x"
            [providers.custom.models."gemini-x"]
            transport = "gemini"
            efforts = ["low"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();

        // The capability belongs to the Gemini transport only.
        let config: Config = toml::from_str(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://example.com/v1"
            default_model = "m"
            [providers.custom.models.m]
            transport = "responses"
            [providers.custom.models.m.gemini_thinking]
            mode = "budget"
            zero_allowed = true
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("only valid with the gemini transport"),
            "{error}"
        );

        // Malformed declarations are rejected before a request.
        for (declaration, expected) in [
            (
                "mode = 'levels'\nlevels = []\n",
                "at least one documented thinking level",
            ),
            (
                "mode = 'levels'\nlevels = ['low', 'low']\n",
                "declared twice",
            ),
            (
                "mode = 'levels'\nlevels = ['low', 'high']\noff = 'minimal'\n",
                "must be one of the declared levels",
            ),
        ] {
            let input = format!(
                r#"
                [providers.custom]
                kind = "openai-compatible"
                base_url = "https://gemini.example.com/v1beta"
                default_model = "gemini-x"
                [providers.custom.models."gemini-x"]
                transport = "gemini"
                [providers.custom.models."gemini-x".gemini_thinking]
                {declaration}"#
            );
            let config: Config = toml::from_str(&input).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(expected), "{input}: {error}");
        }
    }

    #[test]
    fn builtin_models_need_no_copied_effort_mapping() {
        // Absent map: the adapter owns the catalog mapping.
        let config: Config = toml::from_str(
            r#"
            [providers.openai]
            kind = "openai"
            default_model = "gpt-5.5"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert!(
            config.providers["openai"].models.is_empty(),
            "a selection is never a copy of catalog metadata"
        );
    }

    #[test]
    fn extension_lifecycle_has_one_default_source_and_parses_overrides() {
        let defaults = ExtensionLifecycleConfig::default();
        assert_eq!(
            defaults.spawn_seconds,
            crate::extension::DEFAULT_SPAWN_SECONDS
        );
        assert_eq!(
            defaults.initialize_seconds,
            crate::extension::DEFAULT_INITIALIZE_SECONDS
        );
        assert_eq!(
            defaults.ready_seconds,
            crate::extension::DEFAULT_READY_SECONDS
        );
        assert_eq!(
            defaults.request_seconds,
            crate::extension::DEFAULT_REQUEST_SECONDS
        );
        assert_eq!(
            defaults.shutdown_seconds,
            crate::extension::DEFAULT_SHUTDOWN_SECONDS
        );
        assert_eq!(
            defaults.exit_seconds,
            crate::extension::DEFAULT_EXIT_SECONDS
        );

        let config: Config = toml::from_str(
            r#"
            [extension_lifecycle]
            initialize_seconds = 3
            request_seconds = 45
            "#,
        )
        .unwrap();
        let lifecycle: crate::extension::ExtensionLifecycle = (&config.extension_lifecycle).into();
        assert_eq!(lifecycle.initialize, std::time::Duration::from_secs(3));
        assert_eq!(lifecycle.request, std::time::Duration::from_secs(45));
        assert_eq!(
            lifecycle.shutdown,
            std::time::Duration::from_secs(crate::extension::DEFAULT_SHUTDOWN_SECONDS)
        );
    }

    #[test]
    fn extension_lifecycle_rejects_zero_timeouts() {
        for field in [
            "spawn",
            "initialize",
            "ready",
            "request",
            "shutdown",
            "exit",
        ] {
            let input = format!("[extension_lifecycle]\n{field}_seconds = 0\n");
            let error = toml::from_str::<Config>(&input).unwrap_err().to_string();
            assert!(
                error.contains("positive number of seconds"),
                "{field}: {error}"
            );
        }
    }

    #[test]
    fn opencode_go_default_base_url_targets_the_versioned_gateway() {
        // The OpenCode Go gateway is served under /zen/go/v1; a bare /zen/go
        // base 404s every request. Detection (session header, replay) still
        // matches the default because it is a path-boundary prefix check.
        assert_eq!(
            ProviderKind::OpenCodeGo.default_base_url(),
            "https://opencode.ai/zen/go/v1"
        );
        assert!(is_opencode_go_url(
            ProviderKind::OpenCodeGo.default_base_url()
        ));
    }

    #[test]
    fn legacy_config_saves_to_canonical_providers_without_legacy_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            [provider]
            kind = "openai-compatible"
            model = "custom-model"
            base_url = "https://example.com/v1"
            api_key_env = "CUSTOM_KEY"

            [models.custom-model]
            context_window_tokens = 123456

            [models.custom-model.pricing]
            input_per_million = 1.0
            "#,
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        config.save(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("[provider]"),
            "legacy table is gone: {saved}"
        );
        assert!(
            !saved.contains("[models."),
            "legacy global models table is folded into providers: {saved}"
        );
        assert!(saved.contains("[providers.openai-compatible]"), "{saved}");
        assert!(saved.contains("credential = \"env:CUSTOM_KEY\""), "{saved}");
        assert!(saved.contains("[inference]"), "{saved}");

        // The saved canonical config reloads with identical semantics.
        let reloaded = Config::load(Some(&path)).unwrap();
        let registry = crate::providers::ProviderRegistry::from_config(&reloaded).unwrap();
        let descriptor = registry
            .model_descriptor("openai-compatible", "custom-model")
            .unwrap();
        assert_eq!(descriptor.context_window_tokens, Some(123_456));
        assert_eq!(
            descriptor
                .pricing
                .as_ref()
                .and_then(|pricing| pricing.input_per_million),
            Some(1.0)
        );
        assert_eq!(
            registry.default_provider().credential.display(),
            "env:CUSTOM_KEY"
        );
        let (profile, _) = registry.default_profile(&reloaded).unwrap();
        assert_eq!(profile.model, "custom-model");
    }

    #[test]
    fn model_pricing_parses_and_stays_honest_about_missing_components() {
        let config: Config = toml::from_str(
            r#"
            [models.deepseek-flash.pricing]
            input_per_million = 0.28
            output_per_million = 0.42
            currency = "USD"
            "#,
        )
        .unwrap();
        let pricing = config.pricing_for("deepseek-flash").expect("pricing");
        assert_eq!(pricing.input_per_million, Some(0.28));
        assert_eq!(pricing.output_per_million, Some(0.42));
        assert_eq!(pricing.cache_read_per_million, None);
        assert_eq!(pricing.cache_write_per_million, None);
        assert_eq!(pricing.currency, "USD");
        assert!(config.pricing_for("unknown-model").is_none());
    }
}
