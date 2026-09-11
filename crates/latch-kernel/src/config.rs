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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_active_budget")]
    pub active_bytes: usize,
    #[serde(default = "default_recent_budget")]
    pub recent_bytes: usize,
    #[serde(default = "default_reserve")]
    pub reserve_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureConfig {
    #[serde(default = "default_retry")]
    pub retry_budget: u32,
    /// Consecutive model turns that only repeat unchanged inspections are
    /// tolerated before the kernel re-grounds the model.
    #[serde(default = "default_stagnation")]
    pub stagnation_budget: u32,
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
fn default_active_budget() -> usize {
    96_000
}
fn default_recent_budget() -> usize {
    48_000
}
fn default_reserve() -> usize {
    16_000
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
            active_bytes: default_active_budget(),
            recent_bytes: default_recent_budget(),
            reserve_bytes: default_reserve(),
        }
    }
}
impl Default for FailureConfig {
    fn default() -> Self {
        Self {
            retry_budget: default_retry(),
            stagnation_budget: default_stagnation(),
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
        assert_eq!(config.context.active_bytes, 96_000);
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
