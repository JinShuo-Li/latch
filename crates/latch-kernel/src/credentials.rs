//! Credential references and secure local storage.
//!
//! A configuration never contains secret material. It contains a symbolic
//! [`CredentialRef`] such as `env:DEEPSEEK_API_KEY` or `file:deepseek`. Values
//! live in the process environment or in a dedicated secrets file under the
//! Latch state directory with `0600` permissions. Credentials are resolved
//! freshly for each process start; they are never written to the durable event
//! log, the model context, the transcript, or ordinary logs.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Where one provider's API key comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialRef {
    /// Read the value from an environment variable at process start.
    Env(String),
    /// Read the value from the local secrets file under the given key.
    File(String),
    /// Read the value from the OS credential store.
    Keyring(String),
}

impl CredentialRef {
    /// The symbolic reference as written in configuration.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Env(name) => format!("env:{name}"),
            Self::File(name) => format!("file:{name}"),
            Self::Keyring(name) => format!("keyring:{name}"),
        }
    }

    /// The name after the scheme prefix, used for UI labels.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Env(name) | Self::File(name) | Self::Keyring(name) => name,
        }
    }
}

impl std::fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

impl FromStr for CredentialRef {
    type Err = String;

    /// Parses `env:NAME`, `file:NAME`, or `keyring:NAME`. A bare value without
    /// a recognized scheme is rejected: a raw API key pasted into a config
    /// field must never be accepted or persisted.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let raw = raw.trim();
        let (scheme, name) = raw.split_once(':').ok_or_else(|| {
            format!("credential {raw:?} must be env:NAME, file:NAME, or keyring:NAME")
        })?;
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("credential {raw:?} has an empty name"));
        }
        if name.contains(char::is_whitespace) {
            return Err(format!("credential {raw:?} contains whitespace"));
        }
        match scheme.trim().to_ascii_lowercase().as_str() {
            "env" => Ok(Self::Env(name.to_owned())),
            "file" => Ok(Self::File(name.to_owned())),
            "keyring" => Ok(Self::Keyring(name.to_owned())),
            other => Err(format!(
                "unknown credential scheme {other:?}; use env:, file:, or keyring:"
            )),
        }
    }
}

/// Local secrets file layout:
///
/// ```toml
/// [secrets]
/// deepseek = "sk-..."
/// ```
#[derive(Deserialize, Serialize, Default)]
struct SecretsFile {
    #[serde(default)]
    secrets: BTreeMap<String, String>,
}

/// Read-only view plus writer for the local secrets file. `Debug` is redacted
/// so a store can never leak secret material through logging.
pub struct CredentialStore {
    path: PathBuf,
    values: BTreeMap<String, String>,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialStore")
            .field("path", &self.path)
            .field("entries", &self.values.len())
            .finish()
    }
}

impl CredentialStore {
    /// Default path for the local secrets file under the Latch state dir.
    #[must_use]
    pub fn default_path(state_dir: &Path) -> PathBuf {
        state_dir.join("secrets.toml")
    }

    /// Opens the secrets file. A missing file is an empty store, not an error.
    /// On Unix, a file that is group/world accessible is refused with an
    /// actionable message so a plaintext key is never read from a loose file.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let values = if path.exists() {
            check_restrictive(&path)?;
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            if text.trim().is_empty() {
                BTreeMap::new()
            } else {
                toml::from_str::<SecretsFile>(&text)
                    .with_context(|| format!("parse {}", path.display()))?
                    .secrets
            }
        } else {
            BTreeMap::new()
        };
        Ok(Self { path, values })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolves a reference to its value when available.
    pub fn resolve(&self, reference: &CredentialRef) -> Result<Option<String>> {
        match reference {
            CredentialRef::Env(name) => Ok(std::env::var(name).ok().filter(|v| !v.is_empty())),
            CredentialRef::File(name) => Ok(self.values.get(name).cloned()),
            CredentialRef::Keyring(_) => bail!(
                "OS keyring credentials are not available in this build; \
                 use env:NAME or file:NAME (stored 0600 under the Latch state dir)"
            ),
        }
    }

    /// Resolves a reference or returns an actionable missing-credential error.
    pub fn require(&self, reference: &CredentialRef) -> Result<String> {
        self.resolve(reference)?.ok_or_else(|| {
            anyhow!(
                "no credential for {}: {} is not set (run `/setup` or set it in the environment)",
                reference.display(),
                match reference {
                    CredentialRef::Env(name) => format!("environment variable {name}"),
                    CredentialRef::File(name) =>
                        format!("secret {name:?} in {}", self.path.display()),
                    CredentialRef::Keyring(name) => format!("keyring entry {name}"),
                }
            )
        })
    }

    /// Stores a secret under `name` and flushes the file with `0600` mode.
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("secret name must not be empty");
        }
        if value.is_empty() {
            bail!("secret value must not be empty");
        }
        self.values.insert(name.to_owned(), value.to_owned());
        self.flush()
    }

    /// Removes a secret. Returns whether it existed.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let existed = self.values.remove(name).is_some();
        if existed {
            self.flush()?;
        }
        Ok(existed)
    }

    fn flush(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let text = toml::to_string(&SecretsFile {
            secrets: self.values.clone(),
        })
        .context("serialize secrets")?;
        write_restricted(&self.path, &text)
    }
}

#[cfg(unix)]
fn check_restrictive(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        bail!(
            "secrets file {} is group/world accessible (mode {:o}); run `chmod 600 {}`",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_restrictive(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn write_restricted(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temp = path.with_extension("toml.tmp");
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("write {}", temp.display()))?;
        file.flush()?;
    }
    std::fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_restricted(path: &Path, text: &str) -> Result<()> {
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, text).with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Replaces every occurrence of any known secret with `[redacted]`. Used on
/// provider error bodies and diagnostics so a credential echoed by an endpoint
/// can never reach the transcript or logs.
#[must_use]
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        if !secret.is_empty() && out.contains(secret) {
            out = out.replace(secret, "[redacted]");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_references_parse_and_reject_raw_keys() {
        assert_eq!(
            "env:DEEPSEEK_API_KEY".parse::<CredentialRef>().unwrap(),
            CredentialRef::Env("DEEPSEEK_API_KEY".into())
        );
        assert_eq!(
            "file:deepseek".parse::<CredentialRef>().unwrap(),
            CredentialRef::File("deepseek".into())
        );
        assert_eq!(
            "keyring:deepseek".parse::<CredentialRef>().unwrap(),
            CredentialRef::Keyring("deepseek".into())
        );
        assert!("sk-live-12345".parse::<CredentialRef>().is_err());
        assert!("env:".parse::<CredentialRef>().is_err());
        assert!("vault:x".parse::<CredentialRef>().is_err());
    }

    #[test]
    fn secrets_file_round_trips_with_restrictive_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        let mut store = CredentialStore::open(&path).unwrap();
        assert!(
            store
                .resolve(&CredentialRef::File("deepseek".into()))
                .unwrap()
                .is_none()
        );
        store.set("deepseek", "sk-test-secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "secrets file must be 0600");
        }
        let reopened = CredentialStore::open(&path).unwrap();
        assert_eq!(
            reopened
                .require(&CredentialRef::File("deepseek".into()))
                .unwrap(),
            "sk-test-secret"
        );
        // Debug output never contains the value.
        let debug = format!("{reopened:?}");
        assert!(!debug.contains("sk-test-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn loose_secret_files_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "[secrets]\nx = \"y\"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = CredentialStore::open(&path).unwrap_err().to_string();
        assert!(error.contains("chmod 600"), "{error}");
    }

    #[test]
    fn missing_env_credentials_report_the_variable() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::open(dir.path().join("secrets.toml")).unwrap();
        let reference = CredentialRef::Env("LATCH_TEST_SURELY_MISSING_KEY".into());
        assert!(store.resolve(&reference).unwrap().is_none());
        let error = store.require(&reference).unwrap_err().to_string();
        assert!(error.contains("LATCH_TEST_SURELY_MISSING_KEY"));
    }

    #[test]
    fn redaction_removes_secret_material() {
        let text = "HTTP 401: key sk-abc rejected for sk-abc";
        assert_eq!(
            redact(text, &["sk-abc"]),
            "HTTP 401: key [redacted] rejected for [redacted]"
        );
        assert_eq!(redact("no secrets", &["sk-abc"]), "no secrets");
    }
}
