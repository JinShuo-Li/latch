//! Provider registry and model catalog.
//!
//! One centralized layer answers provider/capability/model questions so the
//! kernel loop, CLI, and TUI never inspect base URLs, DeepSeek model names, or
//! wire parameters. The layering is:
//!
//! ```text
//! Provider configuration
//!         -> Provider capabilities
//!         -> Model catalog / metadata
//!         -> InferenceProfile
//!         -> Agent runtime
//!         -> Provider adapter / wire format
//! ```
//!
//! User configuration overrides built-in metadata, which overrides the
//! conservative provider default. Unknown models stay unknown: no invented
//! context window, pricing, cache semantics, or reasoning parameters.

use crate::config::{
    Config, ModelConfig, ProviderKind, ProviderProfileConfig, ReasoningReplayPolicy,
};
use crate::credentials::{CredentialRef, CredentialStore};
use crate::provider::{AnthropicProvider, ModelProvider, OpenAiProvider, ReasoningReplay};
use anyhow::{Result, anyhow, bail};
use latch_protocol::{InferenceProfile, ModelPricing, ProviderId, ReasoningEffort};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

/// Static, conservative facts about one provider wire family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub kind: ProviderKind,
    pub display_name: &'static str,
    pub default_base_url: &'static str,
    pub default_credential: &'static str,
    /// Whether the adapter attaches the stable `x-opencode-session` header.
    pub session_header: bool,
    /// Whether the provider exposes a reliable model-list endpoint. Latch only
    /// records the capability today; remote discovery is a later extension.
    pub model_discovery: bool,
}

impl ProviderCapabilities {
    #[must_use]
    pub fn for_kind(kind: ProviderKind) -> Self {
        Self {
            kind,
            display_name: kind.display_name(),
            default_base_url: kind.default_base_url(),
            default_credential: kind.default_credential(),
            session_header: matches!(kind, ProviderKind::OpenCodeGo),
            model_discovery: false,
        }
    }
}

/// Fully resolved metadata for one model on one provider. Every field may be
/// unknown (`None` / empty), and an unknown field stays unknown.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDescriptor {
    pub provider: ProviderId,
    pub model: String,
    pub display_name: String,
    /// `None` means unknown; callers use the conservative fallback window.
    pub context_window_tokens: Option<usize>,
    /// Efforts the model actually accepts. Empty means the only selectable
    /// state is [`ReasoningEffort::ProviderDefault`].
    pub supported_efforts: Vec<ReasoningEffort>,
    pub default_effort: ReasoningEffort,
    pub reasoning_replay: ReasoningReplay,
    pub pricing: Option<ModelPricing>,
    pub aliases: Vec<String>,
    /// True when metadata comes from the built-in catalog or explicit user
    /// configuration; false for the conservative unknown-model fallback.
    pub known: bool,
}

impl ModelDescriptor {
    #[must_use]
    pub fn supports_effort(&self, effort: ReasoningEffort) -> bool {
        matches!(effort, ReasoningEffort::ProviderDefault)
            || self.supported_efforts.contains(&effort)
    }

    /// The effort actually used for a request: the requested value when
    /// supported, otherwise the model default. Unknown/unsupported values are
    /// never sent on the wire.
    #[must_use]
    pub fn effective_effort(&self, requested: ReasoningEffort) -> ReasoningEffort {
        if self.supports_effort(requested) {
            requested
        } else {
            self.default_effort
        }
    }

    /// Selectable efforts for the UI, ordered low to high, always including
    /// provider default.
    #[must_use]
    pub fn selectable_efforts(&self) -> Vec<ReasoningEffort> {
        let mut efforts = vec![ReasoningEffort::ProviderDefault];
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            if self.supported_efforts.contains(&effort) {
                efforts.push(effort);
            }
        }
        efforts
    }

    /// The conservative descriptor used when neither the built-in catalog nor
    /// user configuration knows the model.
    #[must_use]
    pub fn unknown(provider: &ProviderId, kind: ProviderKind, model: &str) -> Self {
        Self {
            provider: provider.clone(),
            model: model.to_owned(),
            display_name: model.to_owned(),
            context_window_tokens: None,
            supported_efforts: Vec::new(),
            default_effort: ReasoningEffort::ProviderDefault,
            reasoning_replay: default_replay(kind, model),
            pricing: None,
            aliases: Vec::new(),
            known: false,
        }
    }
}

fn default_replay(kind: ProviderKind, model: &str) -> ReasoningReplay {
    let model = model.to_ascii_lowercase();
    match kind {
        ProviderKind::DeepSeek => ReasoningReplay::Replay,
        ProviderKind::OpenCodeGo => {
            if model.contains("deepseek") || model.contains("reasoner") {
                ReasoningReplay::Replay
            } else {
                ReasoningReplay::Omit
            }
        }
        _ => ReasoningReplay::Omit,
    }
}

/// One built-in model row. Only stable, publicly documented facts are listed;
/// pricing is deliberately absent because Latch never invents prices.
struct BuiltinModel {
    id: &'static str,
    display_name: &'static str,
    context_window_tokens: Option<usize>,
    efforts: &'static [ReasoningEffort],
    default_effort: ReasoningEffort,
}

const LOW: ReasoningEffort = ReasoningEffort::Low;
const HIGH: ReasoningEffort = ReasoningEffort::High;
const MAX: ReasoningEffort = ReasoningEffort::Max;

/// Models from the pinned Codex reference (`models-manager/models.json`).
/// Supported efforts are restricted to the provider-neutral Latch set; a
/// provider-default level that is not representable maps to
/// `ProviderDefault` instead of a guessed value.
fn builtin_openai() -> Vec<BuiltinModel> {
    vec![
        BuiltinModel {
            id: "gpt-6-astra",
            display_name: "GPT-6-Astra",
            context_window_tokens: Some(272_000),
            efforts: &[LOW, HIGH, MAX],
            default_effort: ReasoningEffort::ProviderDefault,
        },
        BuiltinModel {
            id: "gpt-5.6-sol",
            display_name: "GPT-5.6-Sol",
            context_window_tokens: Some(272_000),
            efforts: &[LOW, HIGH, MAX],
            default_effort: ReasoningEffort::ProviderDefault,
        },
        BuiltinModel {
            id: "gpt-5.6-terra",
            display_name: "GPT-5.6-Terra",
            context_window_tokens: Some(272_000),
            efforts: &[LOW, HIGH, MAX],
            default_effort: ReasoningEffort::ProviderDefault,
        },
        BuiltinModel {
            id: "gpt-5.6-luna",
            display_name: "GPT-5.6-Luna",
            context_window_tokens: Some(272_000),
            efforts: &[LOW, HIGH, MAX],
            default_effort: ReasoningEffort::ProviderDefault,
        },
        BuiltinModel {
            id: "gpt-5.5",
            display_name: "GPT-5.5",
            context_window_tokens: Some(272_000),
            efforts: &[LOW, HIGH],
            default_effort: ReasoningEffort::ProviderDefault,
        },
        BuiltinModel {
            id: "gpt-5.4",
            display_name: "GPT-5.4",
            context_window_tokens: Some(272_000),
            efforts: &[LOW, HIGH],
            default_effort: ReasoningEffort::ProviderDefault,
        },
    ]
}

fn builtin_anthropic() -> Vec<BuiltinModel> {
    vec![
        BuiltinModel {
            id: "claude-sonnet-4-5",
            display_name: "Claude Sonnet 4.5",
            context_window_tokens: None,
            efforts: &[],
            default_effort: ReasoningEffort::ProviderDefault,
        },
        BuiltinModel {
            id: "claude-opus-4-1",
            display_name: "Claude Opus 4.1",
            context_window_tokens: None,
            efforts: &[],
            default_effort: ReasoningEffort::ProviderDefault,
        },
    ]
}

fn builtin_deepseek() -> Vec<BuiltinModel> {
    vec![
        BuiltinModel {
            id: "deepseek-v4.1-flash",
            display_name: "DeepSeek V4.1 Flash",
            context_window_tokens: None,
            efforts: &[LOW, HIGH, MAX],
            default_effort: ReasoningEffort::ProviderDefault,
        },
        BuiltinModel {
            id: "deepseek-v4.1",
            display_name: "DeepSeek V4.1",
            context_window_tokens: None,
            efforts: &[LOW, HIGH, MAX],
            default_effort: ReasoningEffort::ProviderDefault,
        },
        BuiltinModel {
            id: "deepseek-chat",
            display_name: "DeepSeek Chat",
            context_window_tokens: None,
            efforts: &[],
            default_effort: ReasoningEffort::ProviderDefault,
        },
        BuiltinModel {
            id: "deepseek-reasoner",
            display_name: "DeepSeek Reasoner",
            context_window_tokens: None,
            efforts: &[],
            default_effort: ReasoningEffort::ProviderDefault,
        },
    ]
}

fn builtin_opencode_go() -> Vec<BuiltinModel> {
    vec![
        BuiltinModel {
            id: "deepseek-v4.1-flash",
            display_name: "DeepSeek V4.1 Flash",
            context_window_tokens: None,
            efforts: &[LOW, HIGH, MAX],
            default_effort: ReasoningEffort::ProviderDefault,
        },
        BuiltinModel {
            id: "deepseek-v4.1",
            display_name: "DeepSeek V4.1",
            context_window_tokens: None,
            efforts: &[LOW, HIGH, MAX],
            default_effort: ReasoningEffort::ProviderDefault,
        },
    ]
}

fn builtin_models(kind: ProviderKind) -> Vec<BuiltinModel> {
    match kind {
        ProviderKind::OpenAi => builtin_openai(),
        ProviderKind::Anthropic => builtin_anthropic(),
        ProviderKind::DeepSeek => builtin_deepseek(),
        ProviderKind::OpenCodeGo => builtin_opencode_go(),
        ProviderKind::OpenAiCompatible => Vec::new(),
    }
}

/// One resolved provider instance: identity, endpoint, credential reference,
/// and the merged model catalog.
#[derive(Debug, Clone)]
pub struct ProviderProfile {
    pub id: ProviderId,
    pub display_name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub credential: CredentialRef,
    pub default_model: String,
    pub capabilities: ProviderCapabilities,
    pub model_discovery: bool,
    models: BTreeMap<String, ModelDescriptor>,
    aliases: BTreeMap<String, String>,
}

impl ProviderProfile {
    /// Merges built-in metadata, user model overrides, and the conservative
    /// fallback with precedence: explicit user config > built-in > conservative.
    fn build(
        id: &str,
        entry: &ProviderProfileConfig,
        global_models: &BTreeMap<String, ModelConfig>,
    ) -> Result<Self> {
        let kind = entry.kind;
        let provider_id = ProviderId::new(id);
        let base_url = entry
            .base_url
            .clone()
            .map(|url| url.trim_end_matches('/').to_owned())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| kind.default_base_url().to_owned());
        if base_url.is_empty() {
            bail!("provider {id} needs a base_url");
        }
        let credential: CredentialRef = entry
            .credential
            .clone()
            .unwrap_or_else(|| kind.default_credential().to_owned())
            .parse()
            .map_err(|error| anyhow!("provider {id}: {error}"))?;

        let mut models: BTreeMap<String, ModelDescriptor> = BTreeMap::new();
        let mut aliases: BTreeMap<String, String> = BTreeMap::new();
        for builtin in builtin_models(kind) {
            let descriptor = ModelDescriptor {
                provider: provider_id.clone(),
                model: builtin.id.to_owned(),
                display_name: builtin.display_name.to_owned(),
                context_window_tokens: builtin.context_window_tokens,
                supported_efforts: builtin.efforts.to_vec(),
                default_effort: builtin.default_effort,
                reasoning_replay: default_replay(kind, builtin.id),
                pricing: None,
                aliases: Vec::new(),
                known: true,
            };
            models.insert(builtin.id.to_owned(), descriptor);
        }
        // User entries can extend the catalog with models the built-in table
        // does not know, and can override built-in metadata field by field.
        let mut names: Vec<&String> = entry.models.keys().collect();
        for name in global_models.keys() {
            if !entry.models.contains_key(name) {
                names.push(name);
            }
        }
        for name in names {
            let user = entry.models.get(name).or_else(|| global_models.get(name));
            let Some(user) = user else { continue };
            let mut descriptor = models
                .get(name)
                .cloned()
                .unwrap_or_else(|| ModelDescriptor::unknown(&provider_id, kind, name));
            apply_user_metadata(&mut descriptor, user);
            descriptor.known = true;
            for alias in &descriptor.aliases {
                aliases.insert(alias.clone(), name.clone());
            }
            models.insert(name.clone(), descriptor);
        }

        let default_model = entry
            .default_model
            .clone()
            .filter(|model| !model.trim().is_empty())
            .or_else(|| builtin_models(kind).first().map(|m| m.id.to_owned()))
            .unwrap_or_else(|| "unknown".to_owned());
        Ok(Self {
            id: provider_id,
            display_name: entry
                .display_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| kind.display_name().to_owned()),
            kind,
            base_url,
            credential,
            default_model,
            capabilities: ProviderCapabilities::for_kind(kind),
            model_discovery: entry.model_discovery,
            models,
            aliases,
        })
    }

    /// Canonical model id for a name or alias.
    #[must_use]
    pub fn resolve_model_name(&self, model: &str) -> String {
        let trimmed = model.trim();
        self.aliases
            .get(trimmed)
            .cloned()
            .unwrap_or_else(|| trimmed.to_owned())
    }

    /// Metadata for a model. Unknown models get the conservative descriptor
    /// instead of an invented one.
    #[must_use]
    pub fn model_descriptor(&self, model: &str) -> Option<ModelDescriptor> {
        let canonical = self.resolve_model_name(model);
        if canonical.trim().is_empty() {
            return None;
        }
        Some(
            self.models
                .get(&canonical)
                .cloned()
                .unwrap_or_else(|| ModelDescriptor::unknown(&self.id, self.kind, &canonical)),
        )
    }

    #[must_use]
    pub fn available_models(&self) -> Vec<&ModelDescriptor> {
        self.models.values().collect()
    }
}

fn apply_user_metadata(descriptor: &mut ModelDescriptor, user: &ModelConfig) {
    if let Some(name) = &user.display_name {
        descriptor.display_name = name.clone();
    }
    if let Some(window) = user.context_window_tokens {
        descriptor.context_window_tokens = Some(window);
    }
    if let Some(pricing) = &user.pricing {
        descriptor.pricing = Some(pricing.clone());
    }
    if let Some(efforts) = &user.efforts {
        descriptor.supported_efforts = efforts
            .iter()
            .copied()
            .filter(|effort| !matches!(effort, ReasoningEffort::ProviderDefault))
            .collect();
    }
    if let Some(default_effort) = user.default_effort {
        descriptor.default_effort = default_effort;
    }
    if let Some(policy) = user.reasoning_replay {
        descriptor.reasoning_replay = match policy {
            ReasoningReplayPolicy::Replay => ReasoningReplay::Replay,
            ReasoningReplayPolicy::Omit => ReasoningReplay::Omit,
        };
    }
    if !user.aliases.is_empty() {
        descriptor.aliases = user.aliases.clone();
    }
}

/// Central provider/model catalog.
pub struct ProviderRegistry {
    profiles: BTreeMap<String, ProviderProfile>,
    /// The profile selected when the user has not chosen one.
    default_provider: String,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.profiles.keys().collect::<Vec<_>>())
            .field("default_provider", &self.default_provider)
            .finish()
    }
}

impl ProviderRegistry {
    /// Builds the registry from configuration, migrating the legacy single
    /// `[provider]` table when no `[providers.*]` entries are present.
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut entries: BTreeMap<String, ProviderProfileConfig> = BTreeMap::new();
        if config.providers.is_empty() {
            let legacy = &config.provider;
            let kind = ProviderKind::parse(&legacy.kind, legacy.base_url.as_deref())
                .ok_or_else(|| anyhow!("unsupported provider kind {:?}", legacy.kind))?;
            let credential = legacy
                .api_key_env
                .clone()
                .map(|env| format!("env:{env}"))
                .unwrap_or_else(|| kind.default_credential().to_owned());
            entries.insert(
                kind.id().to_owned(),
                ProviderProfileConfig {
                    kind,
                    display_name: None,
                    base_url: legacy.base_url.clone(),
                    credential: Some(credential),
                    default_model: Some(legacy.model.clone()),
                    models: BTreeMap::new(),
                    model_discovery: false,
                },
            );
        } else {
            entries = config.providers.clone();
        }
        let mut profiles = BTreeMap::new();
        for (id, entry) in &entries {
            let profile = ProviderProfile::build(id, entry, &config.models)?;
            profiles.insert(id.clone(), profile);
        }
        let default_provider = config
            .inference
            .provider
            .clone()
            .filter(|id| profiles.contains_key(id))
            .or_else(|| profiles.keys().next().cloned())
            .ok_or_else(|| anyhow!("no provider is configured"))?;
        Ok(Self {
            profiles,
            default_provider,
        })
    }

    #[must_use]
    pub fn available_providers(&self) -> Vec<&ProviderProfile> {
        self.profiles.values().collect()
    }

    #[must_use]
    pub fn provider(&self, id: &str) -> Option<&ProviderProfile> {
        self.profiles.get(id)
    }

    #[must_use]
    pub fn default_provider(&self) -> &ProviderProfile {
        self.profiles
            .get(&self.default_provider)
            .expect("default provider exists")
    }

    #[must_use]
    pub fn provider_capabilities(&self, id: &str) -> Option<ProviderCapabilities> {
        self.profiles.get(id).map(|p| p.capabilities.clone())
    }

    #[must_use]
    pub fn available_models(&self, id: &str) -> Vec<ModelDescriptor> {
        self.profiles
            .get(id)
            .map(|p| p.available_models().into_iter().cloned().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn model_descriptor(&self, id: &str, model: &str) -> Option<ModelDescriptor> {
        self.profiles.get(id)?.model_descriptor(model)
    }

    #[must_use]
    pub fn supported_efforts(&self, id: &str, model: &str) -> Vec<ReasoningEffort> {
        self.model_descriptor(id, model)
            .map(|descriptor| descriptor.supported_efforts)
            .unwrap_or_default()
    }

    #[must_use]
    pub fn default_effort(&self, id: &str, model: &str) -> ReasoningEffort {
        self.model_descriptor(id, model)
            .map(|descriptor| descriptor.default_effort)
            .unwrap_or_default()
    }

    /// The configured default inference profile, validated against the catalog.
    pub fn default_profile(&self, config: &Config) -> Result<(InferenceProfile, ModelDescriptor)> {
        let provider = config
            .inference
            .provider
            .as_deref()
            .filter(|id| self.profiles.contains_key(*id))
            .unwrap_or_else(|| self.default_provider().id.as_str());
        let profile_models = self.profiles.get(provider).expect("provider checked above");
        let model = config
            .inference
            .model
            .clone()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| profile_models.default_model.clone());
        let descriptor = profile_models
            .model_descriptor(&model)
            .ok_or_else(|| anyhow!("provider {provider} has no default model"))?;
        let effort = descriptor.effective_effort(config.inference.effort);
        Ok((
            InferenceProfile::new(provider, descriptor.model.clone(), effort),
            descriptor,
        ))
    }

    /// Validates a requested profile, resolves aliases, and clamps unsupported
    /// effort values to the model default. Returns the effective profile and
    /// its descriptor.
    pub fn resolve_profile(
        &self,
        requested: &InferenceProfile,
    ) -> Result<(InferenceProfile, ModelDescriptor)> {
        let provider_id = if requested.provider.is_empty() {
            self.default_provider().id.as_str().to_owned()
        } else {
            requested.provider.0.clone()
        };
        let profile = self
            .profiles
            .get(&provider_id)
            .ok_or_else(|| anyhow!("unknown provider {provider_id:?}"))?;
        let model = if requested.model.trim().is_empty() {
            profile.default_model.clone()
        } else {
            profile.resolve_model_name(&requested.model)
        };
        let descriptor = profile
            .model_descriptor(&model)
            .ok_or_else(|| anyhow!("provider {provider_id} has no selectable model"))?;
        let effort = descriptor.effective_effort(requested.effort);
        Ok((
            InferenceProfile::new(provider_id, descriptor.model.clone(), effort),
            descriptor,
        ))
    }

    /// Builds the provider adapter for a resolved profile. Credentials are
    /// resolved here and never stored in the profile or the registry.
    pub fn build_provider(
        &self,
        profile: &InferenceProfile,
        descriptor: &ModelDescriptor,
        credentials: &CredentialStore,
        session_id: Uuid,
    ) -> Result<Arc<dyn ModelProvider>> {
        let provider = self
            .profiles
            .get(profile.provider.as_str())
            .ok_or_else(|| anyhow!("unknown provider {:?}", profile.provider))?;
        let api_key = credentials.require(&provider.credential)?;
        let effort = descriptor.effective_effort(profile.effort);
        let provider_impl: Arc<dyn ModelProvider> = match provider.kind {
            ProviderKind::Anthropic => Arc::new(
                AnthropicProvider::new(
                    provider.base_url.clone(),
                    api_key,
                    descriptor.model.clone(),
                )
                .with_identity(provider.id.to_string()),
            ),
            ProviderKind::OpenAi
            | ProviderKind::OpenAiCompatible
            | ProviderKind::DeepSeek
            | ProviderKind::OpenCodeGo => Arc::new(
                OpenAiProvider::new(provider.base_url.clone(), api_key, descriptor.model.clone())
                    .with_identity(provider.id.to_string())
                    .with_reasoning(
                        effort,
                        descriptor.reasoning_replay,
                        !descriptor.supported_efforts.is_empty(),
                    )
                    .with_session(session_id),
            ),
        };
        Ok(provider_impl)
    }
}

/// Convenience used by the CLI/`/setup` writer: the canonical provider table
/// for one provider kind with its default base URL and credential reference.
#[must_use]
pub fn canonical_provider_entry(
    kind: ProviderKind,
    base_url: Option<String>,
) -> ProviderProfileConfig {
    ProviderProfileConfig {
        kind,
        display_name: None,
        base_url,
        credential: Some(kind.default_credential().to_owned()),
        default_model: None,
        models: BTreeMap::new(),
        model_discovery: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn registry(toml: &str) -> (Config, ProviderRegistry) {
        let config: Config = toml::from_str(toml).unwrap();
        let registry = ProviderRegistry::from_config(&config).unwrap();
        (config, registry)
    }

    #[test]
    fn legacy_single_provider_migrates_into_the_registry() {
        let (config, registry) = registry(
            r#"
            [provider]
            kind = "openai-compatible"
            model = "custom-model"
            base_url = "https://generic.example.com/v1"
            api_key_env = "CUSTOM_KEY"
            "#,
        );
        let profile = registry.default_provider();
        assert_eq!(profile.id.as_str(), "openai-compatible");
        assert_eq!(profile.kind, ProviderKind::OpenAiCompatible);
        assert_eq!(profile.credential.display(), "env:CUSTOM_KEY");
        let (resolved, descriptor) = registry.default_profile(&config).unwrap();
        assert_eq!(resolved.model, "custom-model");
        assert!(!descriptor.known, "unknown custom model stays conservative");
        assert!(descriptor.supported_efforts.is_empty());
        assert!(descriptor.context_window_tokens.is_none());
        assert!(descriptor.pricing.is_none());
        assert_eq!(descriptor.reasoning_replay, ReasoningReplay::Omit);
    }

    #[test]
    fn legacy_opencode_base_url_migrates_to_the_explicit_profile() {
        let (config, registry) = registry(
            r#"
            [provider]
            kind = "openai-compatible"
            model = "deepseek-v4.1-flash"
            base_url = "https://opencode.ai/zen/go"
            api_key_env = "OPENCODE_API_KEY"
            "#,
        );
        let profile = registry.default_provider();
        assert_eq!(profile.kind, ProviderKind::OpenCodeGo);
        assert!(profile.capabilities.session_header);
        let (resolved, descriptor) = registry.default_profile(&config).unwrap();
        assert_eq!(resolved.provider.as_str(), "opencode-go");
        assert_eq!(
            descriptor.reasoning_replay,
            ReasoningReplay::Replay,
            "OpenCode Go serving a DeepSeek model keeps required replay"
        );
    }

    #[test]
    fn multi_provider_config_resolves_profiles_and_precedence() {
        let (config, registry) = registry(
            r#"
            [providers.opencode-go]
            kind = "opencode-go"
            credential = "env:OPENCODE_API_KEY"

            [providers.opencode-go.models."deepseek-v4.1-flash"]
            display_name = "V4 Flash"
            context_window_tokens = 200000
            efforts = ["low", "high", "max"]
            default_effort = "high"

            [providers.deepseek]
            kind = "deepseek"
            credential = "file:deepseek"

            [inference]
            provider = "deepseek"
            model = "deepseek-v4.1"
            effort = "max"
            "#,
        );
        assert_eq!(registry.available_providers().len(), 2);
        let (resolved, _descriptor) = registry.default_profile(&config).unwrap();
        assert_eq!(resolved.provider.as_str(), "deepseek");
        assert_eq!(resolved.effort, ReasoningEffort::Max);

        // Explicit user metadata overrides the built-in row.
        let flash = registry
            .model_descriptor("opencode-go", "deepseek-v4.1-flash")
            .unwrap();
        assert_eq!(flash.display_name, "V4 Flash");
        assert_eq!(flash.context_window_tokens, Some(200_000));
        assert_eq!(flash.default_effort, ReasoningEffort::High);

        // The other provider's model keeps the built-in default.
        let plain = registry
            .model_descriptor("deepseek", "deepseek-v4.1-flash")
            .unwrap();
        assert_eq!(plain.display_name, "DeepSeek V4.1 Flash");
        assert_eq!(plain.context_window_tokens, None);
    }

    #[test]
    fn unknown_models_never_invent_capabilities() {
        let (_config, registry) = registry(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://example.com/v1"
            credential = "env:CUSTOM_KEY"
            "#,
        );
        let descriptor = registry.model_descriptor("custom", "mystery-1").unwrap();
        assert!(!descriptor.known);
        assert!(descriptor.supported_efforts.is_empty());
        assert_eq!(
            descriptor.selectable_efforts(),
            vec![ReasoningEffort::ProviderDefault]
        );
        assert!(descriptor.context_window_tokens.is_none());
        assert!(descriptor.pricing.is_none());
        assert_eq!(descriptor.reasoning_replay, ReasoningReplay::Omit);
    }

    #[test]
    fn effort_resolution_clamps_unsupported_values() {
        let (_config, registry) = registry(
            r#"
            [providers.openai]
            kind = "openai"
            credential = "env:OPENAI_API_KEY"
            "#,
        );
        let (low, descriptor) = registry
            .resolve_profile(&InferenceProfile::new(
                "openai",
                "gpt-5.5",
                ReasoningEffort::Low,
            ))
            .unwrap();
        assert_eq!(low.effort, ReasoningEffort::Low);
        assert!(descriptor.supports_effort(ReasoningEffort::Low));

        // `max` is not advertised by gpt-5.5 and must never be emitted.
        let (clamped, _) = registry
            .resolve_profile(&InferenceProfile::new(
                "openai",
                "gpt-5.5",
                ReasoningEffort::Max,
            ))
            .unwrap();
        assert_eq!(clamped.effort, ReasoningEffort::ProviderDefault);

        // A model with no effort controls only ever selects provider default.
        let (chat, chat_descriptor) = registry
            .resolve_profile(&InferenceProfile::new(
                "openai",
                "mystery-1",
                ReasoningEffort::Max,
            ))
            .unwrap();
        assert_eq!(chat.effort, ReasoningEffort::ProviderDefault);
        assert!(chat_descriptor.supported_efforts.is_empty());
    }

    #[test]
    fn aliases_resolve_to_the_canonical_model() {
        let (_config, registry) = registry(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://example.com/v1"
            credential = "env:CUSTOM_KEY"

            [providers.custom.models."my-model"]
            display_name = "My Model"
            aliases = ["mine"]
            "#,
        );
        let (profile, descriptor) = registry
            .resolve_profile(&InferenceProfile::new(
                "custom",
                "mine",
                ReasoningEffort::ProviderDefault,
            ))
            .unwrap();
        assert_eq!(profile.model, "my-model");
        assert_eq!(descriptor.display_name, "My Model");
    }

    #[test]
    fn provider_entry_can_extend_the_builtin_catalog() {
        let (_config, registry) = registry(
            r#"
            [providers.deepseek]
            kind = "deepseek"
            credential = "file:ds"

            [providers.deepseek.models."future-model"]
            context_window_tokens = 128000
            efforts = ["low", "high"]
            default_effort = "low"
            reasoning_replay = "replay"
            "#,
        );
        let descriptor = registry
            .model_descriptor("deepseek", "future-model")
            .unwrap();
        assert!(descriptor.known);
        assert_eq!(descriptor.context_window_tokens, Some(128_000));
        assert_eq!(
            descriptor.supported_efforts,
            vec![ReasoningEffort::Low, ReasoningEffort::High]
        );
        assert_eq!(descriptor.reasoning_replay, ReasoningReplay::Replay);
    }
}
