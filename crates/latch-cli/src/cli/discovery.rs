//! Conservative OpenCode Go/Zen model discovery.
//!
//! Only ids cross into TUI state. The response never supplies transport or
//! capability facts, and a failed refresh does not alter configuration.

use anyhow::{Context, Result, bail};
use latch_kernel::config::ProviderKind;
use latch_kernel::credentials::CredentialStore;
use latch_kernel::providers::ProviderProfile;
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelId>,
}

#[derive(Deserialize)]
struct ModelId {
    id: String,
}

fn parse_ids(bytes: &[u8]) -> Result<Vec<String>> {
    let list: ModelList = serde_json::from_slice(bytes).context("invalid model list response")?;
    let mut ids: Vec<String> = list
        .data
        .into_iter()
        .map(|model| model.id)
        .filter(|id| !id.trim().is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        bail!("model list was empty");
    }
    Ok(ids)
}

pub async fn fetch(
    profile: &ProviderProfile,
    credentials: &CredentialStore,
) -> Result<Vec<String>> {
    if !matches!(
        profile.kind,
        ProviderKind::OpenCodeGo | ProviderKind::OpenCodeZen
    ) {
        bail!("model discovery is only available for OpenCode Go and Zen");
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()?;
    let url = format!("{}/models", profile.base_url.trim_end_matches('/'));
    let mut request = client.get(url);
    if let Some(secret) = credentials.resolve(&profile.credential)? {
        request = request.bearer_auth(secret);
    }
    let response = request.send().await?.error_for_status()?;
    let bytes = response.bytes().await?;
    parse_ids(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ids_only_and_rejects_empty_or_malformed_responses() {
        let ids = parse_ids(
            br#"{"data":[{"id":"z","transport":"invented"},{"id":"a"},{"id":"z"},{"id":""}]}"#,
        )
        .unwrap();
        assert_eq!(ids, ["a", "z"]);
        assert!(parse_ids(br#"{"data":[]}"#).is_err());
        assert!(parse_ids(br#"{"models":[{"id":"a"}]}"#).is_err());
    }
}
