use anyhow::{Context, Result};
use latch_protocol::{Mode, ModelPricing, PermissionMode, ReasoningEffort, Safety};
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
}

impl TransportKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "messages",
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
            Self::OpenCodeGo => "https://opencode.ai/zen/go",
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
            Self::DeepSeek | Self::OpenCodeGo | Self::OpenAiCompatible => {
                TransportKind::ChatCompletions
            }
        }
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
    dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from(".local/state"))
        .join("latch")
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
            models: BTreeMap::new(),
        }
    }
}

impl Config {
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
        dirs::config_dir().map(|p| p.join("latch/config.toml"))
    }

    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path.map(PathBuf::from).or_else(Self::default_path) else {
            return Ok(Self::default());
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        toml::from_str(
            &std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))
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
                    models: BTreeMap::new(),
                    model_discovery: false,
                },
            );
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
        self.provider = ProviderConfig::default();
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut canonical = self.clone();
        canonical.normalize();
        let text = toml::to_string_pretty(&canonical).context("serialize config")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let temp = path.with_extension("toml.tmp");
        std::fs::write(&temp, text).with_context(|| format!("write {}", temp.display()))?;
        std::fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))?;
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
