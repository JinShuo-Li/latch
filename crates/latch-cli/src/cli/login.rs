//! Human-initiated subscription sign-in. No agent/model can invoke this flow.
use super::{command::Args, session::InferenceContext};
use anyhow::{Result, anyhow, bail};
use latch_kernel::{
    Config, ProviderRegistry,
    config::{InferenceConfig, ProviderKind},
    oauth::ChatGptAuth,
};
use latch_protocol::ReasoningEffort;
use tokio_util::sync::CancellationToken;

/// Publish configuration only after credentials have committed. Keep other
/// configured providers and their new-session default intact.
pub fn configure(context: &mut InferenceContext) -> Result<()> {
    let mut candidate = context.config_for_provider_addition()?;
    let path = context
        .config_path
        .clone()
        .or_else(Config::default_path)
        .ok_or_else(|| anyhow!("cannot resolve configuration path"))?;
    let exists = path.exists();
    let id = ProviderKind::OpenAiCodex.id();
    if candidate
        .providers
        .get(id)
        .is_some_and(|p| p.kind != ProviderKind::OpenAiCodex)
    {
        bail!(
            "provider id openai-codex is already used by another provider; rename it in /setup before signing in"
        );
    }
    let entry = candidate.providers.entry(id.into()).or_insert_with(|| {
        latch_kernel::providers::canonical_provider_entry(ProviderKind::OpenAiCodex, None)
    });
    entry.credential = Some(ProviderKind::OpenAiCodex.default_credential().into());
    entry.base_url = None;
    if entry.default_model.as_deref().is_none_or(str::is_empty) {
        entry.default_model = Some(latch_kernel::oauth::DEFAULT_MODEL.into());
    }
    let model = entry.default_model.clone().expect("default assigned");
    if !exists || candidate.inference.provider.is_none() {
        candidate.inference = InferenceConfig {
            provider: Some(id.into()),
            model: Some(model),
            effort: ReasoningEffort::ProviderDefault,
        };
    }
    candidate.validate()?;
    let registry = ProviderRegistry::from_config(&candidate)?;
    candidate.save(&path)?;
    context.config_path = Some(path);
    context.config = candidate;
    context.registry = registry;
    Ok(())
}

pub async fn execute(args: &Args, logout: bool) -> Result<()> {
    let config = Config::load(args.config.as_deref())?;
    let path = args.config.clone().or_else(Config::default_path);
    let auth = ChatGptAuth::new(&config.state_dir);
    if logout {
        auth.logout().await?;
        println!("Signed out of ChatGPT locally. Provider configuration and history were kept.");
        return Ok(());
    }
    let cancel = CancellationToken::new();
    let login = async {
        let device = auth.begin().await?;
        println!(
            "Sign in to ChatGPT at {}\nEnter code: {}\nOnly enter this code if you initiated this login. Expires in 15 minutes. Ctrl+C cancels.",
            latch_kernel::oauth::VERIFICATION_URL,
            device.user_code
        );
        auth.complete(device, cancel.clone()).await?;
        let mut context = InferenceContext::new(config, path)?;
        configure(&mut context)?;
        println!(
            "ChatGPT subscription connected. Select openai-codex with /model. Plan eligibility and usage limits apply; OpenAI API billing is separate."
        );
        Ok(())
    };
    tokio::select! {
        result = login => result,
        _ = tokio::signal::ctrl_c() => { cancel.cancel(); bail!("ChatGPT sign-in cancelled"); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn login_keeps_the_legacy_account_and_its_effective_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config: Config = toml::from_str(
            r#"
            [provider]
            kind = "openai-compatible"
            base_url = "https://opencode.ai/zen/go/v1"
            api_key_env = "GO_KEY"
            model = "deepseek-v4-flash"
        "#,
        )
        .unwrap();
        config.state_dir = dir.path().join("state");
        config.save(&path).unwrap();
        let before = ProviderRegistry::from_config(&config)
            .unwrap()
            .default_profile(&config)
            .unwrap();
        let mut context = InferenceContext::new(config, Some(path.clone())).unwrap();
        configure(&mut context).unwrap();
        assert_eq!(
            context.registry.default_profile(&context.config).unwrap(),
            before
        );
        assert!(context.config.providers.contains_key("opencode-go"));
        assert!(context.config.providers.contains_key("openai-codex"));
        let restored = Config::load(Some(&path)).unwrap();
        assert_eq!(
            ProviderRegistry::from_config(&restored)
                .unwrap()
                .default_profile(&restored)
                .unwrap(),
            before
        );
    }

    #[test]
    fn first_login_configures_a_new_installation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config {
            state_dir: dir.path().join("state"),
            ..Config::default()
        };
        let mut context = InferenceContext::new(config, Some(path.clone())).unwrap();
        configure(&mut context).unwrap();
        assert_eq!(
            context.config.inference.provider.as_deref(),
            Some("openai-codex")
        );
        assert_eq!(context.config.inference.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(context.config.providers.len(), 1);
        assert_eq!(
            Config::load(Some(&path)).unwrap().providers["openai-codex"].kind,
            ProviderKind::OpenAiCodex
        );
    }

    #[test]
    fn failed_configuration_does_not_publish_live_state() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            ..Config::default()
        };
        let path = dir.path().join("blocked");
        std::fs::create_dir(&path).unwrap();
        let mut context = InferenceContext::new(config, Some(path)).unwrap();
        assert!(configure(&mut context).is_err());
        assert!(!context.config.providers.contains_key("openai-codex"));
    }

    #[test]
    fn login_configuration_is_symbolic_and_preserves_other_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config {
            state_dir: dir.path().join("state"),
            ..Config::default()
        };
        config.providers.insert(
            "deepseek".into(),
            latch_kernel::providers::canonical_provider_entry(ProviderKind::DeepSeek, None),
        );
        config.providers.get_mut("deepseek").unwrap().default_model =
            Some("deepseek-v4.1-flash".into());
        config.inference.provider = Some("deepseek".into());
        config.inference.model = Some("deepseek-v4.1-flash".into());
        config.save(&path).unwrap();
        let mut context = InferenceContext::new(config, Some(path.clone())).unwrap();
        configure(&mut context).unwrap();
        assert!(context.config.providers.contains_key("deepseek"));
        assert_eq!(
            context.config.inference.provider.as_deref(),
            Some("deepseek")
        );
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("oauth:openai-codex"));
        assert!(!text.contains("access_token"));
        assert_eq!(
            context.registry.provider("openai-codex").unwrap().kind,
            ProviderKind::OpenAiCodex
        );
    }
}
