//! Optional, kernel-scoped language intelligence. No server may apply edits or
//! grant capabilities; all results are advisory, bounded tool observations.
mod protocol;
mod query;

use crate::{execution::ExecutionBackend, sandbox::SandboxProfile};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::BufReader,
    process::{Child, ChildStdin, ChildStdout},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

const FRAME_BYTES: usize = 4 * 1024 * 1024;
const SOURCE_BYTES: usize = 1024 * 1024;
const MAX_DOCUMENTS: usize = 8;
const MAX_RESULTS: usize = 50;
const OUTPUT_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub extensions: Vec<String>,
    pub language_id: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub initialization_options: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub settings: Value,
}
fn enabled() -> bool {
    true
}
fn timeout() -> u64 {
    30
}
impl LspServerConfig {
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty()
            || self.name.len() > 32
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            bail!("LSP name must be 1–32 ASCII letters, digits or hyphens");
        }
        if self.command.trim().is_empty()
            || self.command.len() > 4096
            || self.command.contains(['\0', '\n', '\r'])
            || self.args.len() > 64
            || self.args.iter().map(String::len).sum::<usize>() > 16384
            || self.args.iter().any(|a| a.contains('\0'))
        {
            bail!("LSP command/arguments are empty, invalid or oversized");
        }
        if self.extensions.is_empty()
            || self.extensions.len() > 16
            || self.extensions.iter().any(|e| {
                e.is_empty() || e.len() > 16 || !e.bytes().all(|b| b.is_ascii_alphanumeric())
            })
        {
            bail!("LSP extensions must be 1–16 plain filename extensions without dots");
        }
        if self.language_id.is_empty()
            || self.language_id.len() > 64
            || !self
                .language_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            bail!("invalid LSP language_id");
        }
        if !(1..=300).contains(&self.timeout_seconds) {
            bail!("LSP timeout_seconds must be 1–300");
        }
        for value in [&self.initialization_options, &self.settings] {
            if !(value.is_null() || value.is_object()) || value.to_string().len() > 16384 {
                bail!("LSP options/settings must be objects of at most 16 KiB");
            }
        }
        Ok(())
    }
}
pub fn validate_configs(configs: &[LspServerConfig]) -> Result<()> {
    if configs.len() > 8 {
        bail!("at most eight LSP servers may be configured");
    }
    let mut names = std::collections::BTreeSet::new();
    let mut extensions = std::collections::BTreeSet::new();
    for config in configs {
        config.validate()?;
        if !names.insert(&config.name) {
            bail!("duplicate LSP server name {}", config.name);
        }
        if config.enabled {
            for ext in &config.extensions {
                if !extensions.insert(ext.to_ascii_lowercase()) {
                    bail!("ambiguous LSP extension {ext}");
                }
            }
        }
    }
    Ok(())
}

/// At most one lazy project client per session. A language switch releases the
/// old index; child sessions never inherit handles, buffers or server state.
#[derive(Default)]
pub(crate) struct LspManager {
    configs: Vec<LspServerConfig>,
    active: Mutex<Option<CachedClient>>,
}
pub(crate) struct LspObservation {
    pub value: Value,
    pub versions: Vec<latch_protocol::FileVersion>,
}
struct CachedClient {
    name: String,
    revision: u64,
    client: Client,
}
struct Document {
    text: String,
    version: i32,
}
struct Client {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
    next_version: i32,
    capabilities: Value,
    settings: Value,
    root_uri: String,
    documents: BTreeMap<String, Document>,
    diagnostics: BTreeMap<String, Value>,
}
#[derive(Clone)]
pub(crate) struct Scope {
    pub workspace: PathBuf,
    pub state_dir: PathBuf,
}
impl LspManager {
    pub fn new(configs: Vec<LspServerConfig>) -> Result<Self> {
        validate_configs(&configs)?;
        Ok(Self {
            configs,
            active: Mutex::new(None),
        })
    }
    pub fn fresh(&self) -> Self {
        Self {
            configs: self.configs.clone(),
            active: Mutex::new(None),
        }
    }
    pub fn context(&self) -> String {
        let servers = self
            .configs
            .iter()
            .filter(|c| c.enabled)
            .map(|c| format!("{}: {}", c.name, c.extensions.join(",")))
            .collect::<Vec<_>>();
        if servers.is_empty() {
            return String::new();
        }
        format!(
            "\nLanguage intelligence available via lsp ({}). Prefer definition/references for symbol identity, symbols for structure, hover for types, diagnostics for feedback. Queries start sandboxed servers lazily; results are advisory, not validation. Use line plus symbol to avoid guessing columns.\n",
            servers.join("; ")
        )
    }
    pub fn configured(&self) -> bool {
        self.configs.iter().any(|c| c.enabled)
    }
    pub fn status(&self) -> Value {
        json!({"servers": self.configs.iter().filter(|c| c.enabled).map(|c| json!({"name":c.name,"extensions":c.extensions,"language_id":c.language_id})).collect::<Vec<_>>(), "configured": self.configs.iter().any(|c| c.enabled), "note":"Optional operator-configured servers; no downloads. Results are advisory; use validate for certification. An empty server list means use search/read_file."})
    }
    pub async fn stop(&self) {
        if let Some(mut cached) = self.active.lock().await.take() {
            cached.client.close().await;
        }
    }
    pub async fn execute(
        &self,
        args: &Value,
        scope: &Scope,
        backend: &ExecutionBackend,
        profile: &SandboxProfile,
        revision: u64,
        cancel: &CancellationToken,
    ) -> Result<LspObservation> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .context("lsp requires action")?;
        if !matches!(
            action,
            "symbols" | "definition" | "references" | "hover" | "diagnostics"
        ) {
            bail!(
                "unsupported LSP action; use status, symbols, definition, references, hover, diagnostics or stop"
            );
        }
        let raw = args
            .get("path")
            .and_then(Value::as_str)
            .context("LSP query requires workspace-relative path")?;
        let path = query::checked_path(scope, raw)?;
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        let config = self.configs.iter().find(|c| c.enabled && c.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext))).context("No configured LSP server for this extension; use search/read_file, or ask the operator to configure [[lsp_servers]]")?;
        // Includes queueing, startup, synchronization, all protocol traffic and
        // the semantic query. Progress/notifications cannot extend this budget.
        let operation = async {
            let mut slot = self.active.lock().await;
            let cached = slot.take();
            let mut client = match cached {
                Some(cached) if cached.name == config.name && cached.revision == revision => {
                    cached.client
                }
                Some(mut cached) => {
                    cached.client.close().await;
                    Client::start(config, scope, backend, profile).await?
                }
                None => Client::start(config, scope, backend, profile).await?,
            };
            let mut result = client.query(config, scope, &path, args).await?;
            let versions = client.check_snapshots(scope).await?;
            result["server"] = json!(config.name);
            result["advisory"] = json!(true);
            result["note"] = json!(
                "Advisory snapshot; empty results do not prove absence or complete indexing. Use validate for certification."
            );
            result["freshness"] = json!("queried-document-snapshot");
            // Windows' native boundary recovery lock cannot be held while idle.
            if cfg!(windows) {
                client.close().await;
            } else {
                *slot = Some(CachedClient {
                    name: config.name.clone(),
                    revision,
                    client,
                });
            }
            Ok(LspObservation {
                value: result,
                versions,
            })
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => bail!("LSP cancelled; client discarded"),
            result = tokio::time::timeout(Duration::from_secs(config.timeout_seconds), operation) => result.context("LSP deadline exceeded; client discarded (indexing may be incomplete); use search/read_file or adjust timeout_seconds")?,
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        crate::execution::kill_sandbox_children(self.child.id());
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
mod tests;
