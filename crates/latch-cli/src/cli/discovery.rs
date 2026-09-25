//! Conservative OpenCode Go/Zen model discovery.
//!
//! Only ids cross into TUI state. The response never supplies transport or
//! capability facts, and a failed refresh does not alter configuration. The
//! on-disk snapshot lives under the resolved `cache_root` and contains only
//! regenerable availability metadata, so deleting `cache/` is always safe.

use anyhow::{Context, Result, bail};
use latch_kernel::config::ProviderKind;
use latch_kernel::credentials::CredentialStore;
use latch_kernel::providers::ProviderProfile;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Availability evidence older than this is ignored: an old successful refresh
/// is not proof that the provider still serves an id.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

const CACHE_FILE: &str = "discovery.json";

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelId>,
}

#[derive(Deserialize)]
struct ModelId {
    id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DiscoveryCache {
    #[serde(default)]
    providers: BTreeMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    /// Unix seconds of the successful refresh.
    fetched_at: u64,
    ids: Vec<String>,
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
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

/// Reads the cached ids for one provider. Any unreadable, malformed, or stale
/// entry is treated as absent: the cache can never become authoritative.
pub fn cached(cache_root: &Path, provider: &str) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(cache_root.join(CACHE_FILE)).ok()?;
    let cache: DiscoveryCache = serde_json::from_str(&text).ok()?;
    let entry = cache.providers.get(provider)?;
    if entry.ids.is_empty() {
        return None;
    }
    let age = now_seconds().saturating_sub(entry.fetched_at);
    if age > CACHE_TTL.as_secs() {
        return None;
    }
    Some(entry.ids.clone())
}

fn store(cache_root: &Path, provider: &str, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let path = cache_root.join(CACHE_FILE);
    let mut cache: DiscoveryCache = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    cache.providers.insert(
        provider.to_owned(),
        CacheEntry {
            fetched_at: now_seconds(),
            ids: ids.to_vec(),
        },
    );
    std::fs::create_dir_all(cache_root)
        .with_context(|| format!("create {}", cache_root.display()))?;
    let text = serde_json::to_string(&cache).context("serialize discovery cache")?;
    // The snapshot is regenerable, so an interrupted write is recoverable and
    // must never be reported as a successful refresh: write atomically.
    let mut temp = tempfile::NamedTempFile::new_in(cache_root)
        .with_context(|| format!("stage discovery cache in {}", cache_root.display()))?;
    use std::io::Write;
    temp.write_all(text.as_bytes())
        .context("write discovery cache")?;
    temp.as_file().sync_all().context("sync discovery cache")?;
    temp.persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Fetches the live model list and records availability evidence in the
/// resolved cache root. A cache write failure does not invalidate a successful
/// refresh; the caller still receives the ids.
pub async fn fetch(
    profile: &ProviderProfile,
    credentials: &CredentialStore,
    cache_root: &Path,
) -> Result<Vec<String>> {
    let ids = fetch_remote(profile, credentials).await?;
    store(cache_root, profile.id.as_str(), &ids)?;
    Ok(ids)
}

async fn fetch_remote(
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

    #[test]
    fn cache_round_trips_and_ignores_stale_malformed_or_unknown_entries() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), "zen", &["a".into(), "b".into()]).unwrap();
        assert_eq!(
            cached(dir.path(), "zen"),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(cached(dir.path(), "go"), None);

        // A stale snapshot is treated as absent rather than presented as
        // current availability.
        let path = dir.path().join(CACHE_FILE);
        let mut cache: DiscoveryCache =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        cache.providers.get_mut("zen").unwrap().fetched_at = 0;
        std::fs::write(&path, serde_json::to_string(&cache).unwrap()).unwrap();
        assert_eq!(cached(dir.path(), "zen"), None);

        // Malformed and missing files are conservative absences, never errors.
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(cached(dir.path(), "zen"), None);
        assert_eq!(cached(&dir.path().join("missing"), "zen"), None);
        assert_eq!(cached(dir.path(), ""), None);
    }

    #[test]
    fn cache_deletion_is_safe_and_empty_snapshots_are_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), "zen", &[]).unwrap();
        assert_eq!(cached(dir.path(), "zen"), None);
        store(dir.path(), "zen", &["a".into()]).unwrap();
        std::fs::remove_dir_all(dir.path()).unwrap();
        assert_eq!(cached(dir.path(), "zen"), None);
    }
}
