use anyhow::{Context, Result};
use latch_protocol::{Mode, ModelPricing};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub default_mode: Mode,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default)]
    pub permissions: PermissionConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub failure: FailureConfig,
    #[serde(default)]
    pub extensions: Vec<ExtensionConfig>,
    /// Per-model product metadata. Pricing is optional and user-configured;
    /// Latch never fetches or invents provider prices.
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
}

/// Product metadata for one model name, keyed by the provider model string.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
    /// Provider context window in tokens. `None` uses the conservative
    /// [`DEFAULT_CONTEXT_WINDOW_TOKENS`] fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<usize>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionConfig {
    #[serde(default = "yes")]
    pub workspace_write: bool,
    #[serde(default)]
    pub outside_workspace: OutsidePolicy,
    #[serde(default = "default_shell_timeout")]
    pub shell_timeout_seconds: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutsidePolicy {
    #[default]
    Deny,
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
            outside_workspace: OutsidePolicy::Deny,
            shell_timeout_seconds: default_shell_timeout(),
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
            default_mode: Mode::Work,
            state_dir: default_state_dir(),
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

    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path
            .map(PathBuf::from)
            .or_else(|| dirs::config_dir().map(|p| p.join("latch/config.toml")))
        else {
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
