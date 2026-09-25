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
                if let Some(efforts) = &override_.efforts
                    && let Some(default) = override_.default_effort
                    && !efforts.contains(&default)
                {
                    anyhow::bail!(
                        "provider {id} model {model:?} default_effort must be in efforts"
                    );
                }
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
        ] {
            let config: Config = toml::from_str(input).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(expected), "{input}: {error}");
        }
        let config: Config = toml::from_str(base).unwrap();
        config.validate().unwrap();
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
