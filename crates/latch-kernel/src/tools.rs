mod files;
mod git;
mod ownership;
mod policy;
mod process;
mod write;

use ownership::{ChangeLedger, git_dirty_hashes};
pub use policy::{CapabilityGrant, PolicyDecision, PolicyEngine};
use process::ManagedProcess;
pub(crate) use process::is_read_only_shell;

use crate::config::PermissionConfig;
use crate::safety;
use crate::sandbox::{CapabilitySet, SandboxProfile, SandboxRunner};
use crate::store::EventStore;
use crate::tokens::TokenEstimator;
use anyhow::{Context, Result, anyhow, bail};
use latch_protocol::{
    ChangeOwner, EventPayload, FileVersion, Mode, PermissionMode, Safety, ToolCall, ToolResult,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Outcome of the mandatory startup sandbox probe. `Unavailable` carries the
/// actionable refusal message; command execution fails with it rather than
/// silently running unsandboxed.
#[derive(Clone)]
enum SandboxState {
    Ready(SandboxRunner),
    Unavailable(String),
}

#[derive(Clone)]
pub struct ToolExecutor {
    workspace: PathBuf,
    artifacts: PathBuf,
    store: EventStore,
    session_id: Uuid,
    policy: PolicyEngine,
    observations: Arc<Mutex<HashMap<PathBuf, FileVersion>>>,
    /// Content hashes Latch itself has written, per path. Drift onto one of
    /// these is self-authored and must not be mistaken for external change.
    self_authored: Arc<std::sync::Mutex<HashMap<PathBuf, HashSet<String>>>>,
    ledger: Arc<Mutex<ChangeLedger>>,
    mutation_lock: Arc<Mutex<()>>,
    read_slots: Arc<Semaphore>,
    restored: Arc<std::sync::atomic::AtomicBool>,
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    /// Single-use capability grants keyed by kernel call id.
    grants: Arc<std::sync::Mutex<HashMap<String, CapabilityGrant>>>,
    sandbox: Arc<std::sync::RwLock<SandboxState>>,
}
/// Outcome of one bounded shell execution.
#[derive(Debug)]
pub struct ProcessOutput {
    pub success: bool,
    /// Human-readable exit summary, for example `"exit 0"` or `"exit code 101"`.
    pub status_line: String,
    pub text: String,
    pub artifact_id: Option<String>,
    pub elapsed: Duration,
}
impl ProcessOutput {
    /// First diagnostic line for compact transcript rows.
    #[must_use]
    pub fn first_line(&self) -> String {
        self.text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(120)
            .collect()
    }
}
impl ToolExecutor {
    pub fn new(
        workspace: PathBuf,
        artifacts: PathBuf,
        store: EventStore,
        session_id: Uuid,
        policy: PolicyEngine,
    ) -> Result<Self> {
        std::fs::create_dir_all(&artifacts)?;
        let initial = git_dirty_hashes(&workspace).unwrap_or_default();
        let sandbox = match SandboxRunner::detect(&workspace) {
            Ok(runner) => SandboxState::Ready(runner),
            Err(error) => SandboxState::Unavailable(format!("{error:#}")),
        };
        Ok(Self {
            workspace,
            artifacts,
            store,
            session_id,
            policy,
            observations: Arc::new(Mutex::new(HashMap::new())),
            self_authored: Arc::new(std::sync::Mutex::new(HashMap::new())),
            ledger: Arc::new(Mutex::new(ChangeLedger::with_initial(initial))),
            mutation_lock: Arc::new(Mutex::new(())),
            read_slots: Arc::new(Semaphore::new(8)),
            restored: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            processes: Arc::new(Mutex::new(HashMap::new())),
            grants: Arc::new(std::sync::Mutex::new(HashMap::new())),
            sandbox: Arc::new(std::sync::RwLock::new(sandbox)),
        })
    }
    /// Extension hosts run through the same sandbox: read-only workspace,
    /// network for protocol work, masked home. Their own tool semantics remain
    /// a cooperative boundary.
    #[must_use]
    pub fn extension_sandbox_profile(&self) -> SandboxProfile {
        let mut capabilities = CapabilitySet::new();
        capabilities.insert(crate::sandbox::Capability::WorkspaceRead);
        capabilities.insert(crate::sandbox::Capability::NetworkAccess);
        capabilities.insert(crate::sandbox::Capability::ExtensionExecution);
        SandboxProfile::new(
            self.workspace.clone(),
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
            capabilities,
        )
    }
    pub fn sandbox_runner_for_extension(&self) -> Result<SandboxRunner> {
        self.sandbox_runner()
    }
    /// The sandbox is required, not best-effort: a failed probe refuses every
    /// command execution with the probe's actionable message.
    fn sandbox_runner(&self) -> Result<SandboxRunner> {
        let state = self
            .sandbox
            .read()
            .map_err(|_| anyhow!("sandbox state poisoned"))?;
        match &*state {
            SandboxState::Ready(runner) => Ok(runner.clone()),
            SandboxState::Unavailable(message) => bail!("{message}"),
        }
    }
    /// Human-readable sandbox status for startup banners and diagnostics.
    pub fn sandbox_status(&self) -> Result<String> {
        self.sandbox_runner()
            .map(|runner| format!("{} ready", runner.bwrap().display()))
    }
    #[cfg(test)]
    pub(crate) fn force_sandbox_unavailable(&self, message: &str) {
        if let Ok(mut state) = self.sandbox.write() {
            *state = SandboxState::Unavailable(message.to_owned());
        }
    }
    #[cfg(test)]
    pub(crate) fn sandbox_available(&self) -> bool {
        self.sandbox_runner().is_ok()
    }
    /// Capability profile for one call under the current mode and safety
    /// policy, merged with any single-use grant the resolver approved.
    #[must_use]
    pub fn sandbox_profile(&self, call: &ToolCall) -> SandboxProfile {
        let classification = self.policy.classify(&call.name, &call.arguments);
        let mut capabilities = classification.capabilities;
        let mut external_roots = classification.external_roots;
        if let Some(grant) = self.grant_for(&call.id) {
            capabilities.extend(&grant.capabilities);
            external_roots.extend(grant.external_roots);
        }
        SandboxProfile::new(
            self.workspace.clone(),
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
            capabilities,
        )
        .with_external_roots(external_roots)
    }
    /// Records the scoped capability grant a resolver approved for one call.
    /// Grants are single-use and never override a hard deny.
    pub fn grant_call(&self, call_id: &str, grant: CapabilityGrant) {
        if let Ok(mut grants) = self.grants.lock() {
            grants.insert(call_id.to_owned(), grant);
        }
    }

    #[must_use]
    pub fn has_grant(&self, call_id: &str) -> bool {
        self.grants
            .lock()
            .map(|grants| grants.contains_key(call_id))
            .unwrap_or(false)
    }

    fn grant_for(&self, call_id: &str) -> Option<CapabilityGrant> {
        self.grants
            .lock()
            .ok()
            .and_then(|grants| grants.get(call_id).cloned())
    }

    fn clear_grant(&self, call_id: &str) {
        if let Ok(mut grants) = self.grants.lock() {
            grants.remove(call_id);
        }
    }

    /// True when a grant for this call explicitly allows writing `path`.
    pub(crate) fn grant_allows_path(&self, call_id: &str, path: &Path) -> bool {
        let Some(grant) = self.grant_for(call_id) else {
            return false;
        };
        if !grant
            .capabilities
            .contains(crate::sandbox::Capability::ExternalFilesystemWrite)
        {
            return false;
        }
        grant
            .external_roots
            .iter()
            .any(|root| path.starts_with(root))
    }
    pub fn set_mode(&self, mode: Mode) {
        self.policy.set_mode(mode);
    }
    pub fn set_safety(&self, safety: Safety) {
        self.policy.set_safety(safety);
    }
    #[must_use]
    pub fn safety(&self) -> Safety {
        self.policy.safety()
    }
    pub fn set_permissions(&self, mode: PermissionMode) {
        self.policy.set_permissions(mode);
    }
    #[must_use]
    pub fn permissions(&self) -> PermissionMode {
        self.policy.permissions()
    }
    #[must_use]
    pub fn classify_call(&self, tool: &str, args: &Value) -> safety::Classification {
        self.policy.classify(tool, args)
    }
    #[must_use]
    pub fn policy_decision(&self, tool: &str, args: &Value) -> PolicyDecision {
        self.policy.decide(tool, args)
    }

    #[must_use]
    pub fn definitions() -> Vec<latch_protocol::ToolDefinition> {
        vec![
            def(
                "read_file",
                "Read a bounded window of a UTF-8 workspace file and return its version hash. Defaults to the first 400 lines; pass offset (1-based line) and/or limit, or tail, to read another window. The result reports the line range and the offset for continuation, so large files are never injected whole.",
                json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"},"offset":{"type":"integer","description":"1-based first line to return"},"limit":{"type":"integer","description":"maximum lines to return"},"tail":{"type":"integer","description":"return the last N lines instead of a head window"}}}),
            ),
            def(
                "search",
                "Search repository text with ripgrep. Returns a bounded page of matches (default 50) with total count and an offset continuation.",
                json!({"type":"object","required":["query"],"properties":{"query":{"type":"string"},"path":{"type":"string"},"max_results":{"type":"integer","description":"matches per page, default 50"},"offset":{"type":"integer","description":"0-based match offset for continuation"}}}),
            ),
            def(
                "read_artifact",
                "Read a stored artifact (full output spilled by a truncated shell, search, diff, or validation result) by id. Supports the same offset/limit/tail windowing as read_file; the result reports the line range and continuation offset.",
                json!({"type":"object","required":["id"],"properties":{"id":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"},"tail":{"type":"integer"}}}),
            ),
            def(
                "exec_start",
                "Start a persistent development process (server, watcher, long build) in the workspace with bash -lc. Returns a process id for exec_poll and exec_terminate. WORK mode only; policy and dangerous-command checks apply.",
                json!({"type":"object","required":["command"],"properties":{"command":{"type":"string"},"label":{"type":"string","description":"short human label"}}}),
            ),
            def(
                "exec_poll",
                "Return new output from a managed process since the last poll, plus its running/exited status. Exited processes keep their buffered output available through this tool.",
                json!({"type":"object","required":["id"],"properties":{"id":{"type":"string"}}}),
            ),
            def(
                "exec_terminate",
                "Terminate a managed process and return its final status.",
                json!({"type":"object","required":["id"],"properties":{"id":{"type":"string"}}}),
            ),
            def(
                "patch",
                "Guarded exact replacement. base_hash must come from read_file.",
                json!({"type":"object","required":["path","base_hash","old","new"],"properties":{"path":{"type":"string"},"base_hash":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}}}),
            ),
            def(
                "write",
                "Guarded file write. Existing files require a base_hash from read_file; new files use null.",
                json!({"type":"object","required":["path","content"],"properties":{"path":{"type":"string"},"base_hash":{"type":["string","null"]},"content":{"type":"string"}}}),
            ),
            def(
                "shell",
                "Run a bounded Linux developer command. Commands execute with the workspace as the working directory, so `cd <workspace> &&` is redundant — prefer plain `git log --oneline -20`. A `cd` into a subdirectory is allowed for read-only inspection (for example `cd src && rg normalize_username .`), but never `cd` out of the workspace. ASK/PLAN allow only conservative read-only commands and deny test/build execution.",
                json!({"type":"object","required":["command"],"properties":{"command":{"type":"string"},"timeout_seconds":{"type":"integer"}}}),
            ),
            def(
                "git_status",
                "Show concise Git status and diff summary.",
                json!({"type":"object","properties":{}}),
            ),
            def(
                "git_diff",
                "Show the workspace diff, with artifact spill when large.",
                json!({"type":"object","properties":{}}),
            ),
        ]
    }
    pub async fn execute(&self, call: &ToolCall, cancel: CancellationToken) -> ToolResult {
        let classification = self.policy.classify(&call.name, &call.arguments);
        // A resolved `Ask` executes exactly once with its scoped grant. A
        // grant never converts `Deny`: hard deny stays denied.
        let decision = match classification.decision.clone() {
            safety::Decision::Ask(_) if self.grant_for(&call.id).is_some() => PolicyDecision::Allow,
            safety::Decision::Allow => PolicyDecision::Allow,
            safety::Decision::Ask(reason) => PolicyDecision::Ask(reason),
            safety::Decision::Deny(reason) => PolicyDecision::Deny(reason),
        };
        if let Err(error) = self.store.append(
            self.session_id,
            EventPayload::PermissionDecision {
                tool: call.name.clone(),
                decision: format!("{decision:?}"),
                reason: String::new(),
            },
        ) {
            return result(
                call,
                format!("persist permission decision: {error}"),
                true,
                None,
            );
        }
        if let PolicyDecision::Deny(reason) | PolicyDecision::Ask(reason) = decision {
            // Lifecycle invariant: every model-issued ToolRequested(call_id)
            // must eventually have exactly one terminal result. A denial is a
            // terminal outcome even though execution never started, so persist
            // ToolFailed. PermissionDecision above remains the separate audit
            // event; this is the structured result the model sees.
            let denied = result(call, reason, true, None);
            if let Err(error) = self.store.append(
                self.session_id,
                EventPayload::ToolFailed {
                    result: denied.clone(),
                },
            ) {
                return result(
                    call,
                    format!("persist denied tool result: {error}"),
                    true,
                    None,
                );
            }
            return denied;
        }
        if let Err(error) = self.store.append(
            self.session_id,
            EventPayload::ToolStarted {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            },
        ) {
            return result(call, format!("persist tool start: {error}"), true, None);
        }
        let outcome = match call.name.as_str() {
            "read_file" => self.read_file(call).await,
            "search" => self.search(call).await,
            "read_artifact" => self.read_artifact(call).await,
            "exec_start" => self.process_start(call).await,
            "exec_poll" => self.process_poll(call).await,
            "exec_terminate" => self.process_terminate(call).await,
            "patch" => self.patch(call).await,
            "write" => self.write(call).await,
            "shell" => self.shell(call, cancel).await,
            "git_status" => self.git_status(call).await,
            "git_diff" => self.git_diff(call).await,
            "checkpoint" => self.checkpoint(call).await,
            "undo" => self.undo(call).await,
            _ => Err(anyhow!("unknown tool {}", call.name)),
        };
        // The grant is single-use for exactly this call id.
        self.clear_grant(&call.id);
        let r = match outcome {
            Ok(v) => result(call, v.0, false, v.1),
            Err(e) => result(call, format!("{e:#}"), true, None),
        };
        let payload = if r.is_error {
            EventPayload::ToolFailed { result: r.clone() }
        } else {
            EventPayload::ToolCompleted { result: r.clone() }
        };
        if let Err(error) = self.store.append(self.session_id, payload) {
            return result(
                call,
                format!("persist tool result: {error}"),
                true,
                r.artifact_id,
            );
        }
        r
    }
}

fn def(name: &str, description: &str, input_schema: Value) -> latch_protocol::ToolDefinition {
    latch_protocol::ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema,
    }
}
fn result(
    call: &ToolCall,
    output: String,
    is_error: bool,
    artifact_id: Option<String>,
) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        output,
        is_error,
        artifact_id,
    }
}

fn str_arg<'a>(call: &'a ToolCall, name: &str) -> Result<&'a str> {
    call.arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string argument {name}"))
}
fn resolve_workspace_path(workspace: &Path, path: &str) -> Result<PathBuf> {
    let root = workspace.canonicalize()?;
    let candidate = if Path::new(path).is_absolute() {
        lexical_normalize(Path::new(path))
    } else {
        lexical_normalize(&root.join(path))
    };
    if !candidate.starts_with(&root) {
        bail!("path escapes workspace");
    }
    let mut ancestor = candidate.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| anyhow!("invalid path"))?;
    }
    if !ancestor.canonicalize()?.starts_with(&root) {
        bail!("path escapes workspace through a symbolic link");
    }
    if candidate.exists() {
        let resolved = candidate.canonicalize()?;
        if !resolved.starts_with(&root) {
            bail!("path escapes workspace through a symbolic link");
        }
        return Ok(resolved);
    }
    Ok(candidate)
}
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}
fn relative(root: &Path, path: &Path) -> Result<String> {
    let root = root.canonicalize()?;
    // A path outside the workspace only reaches here after an explicit human
    // approval; record it absolutely so provenance and undo stay honest.
    match path.strip_prefix(&root) {
        Ok(relative) => Ok(relative.to_string_lossy().into_owned()),
        Err(_) => Ok(path.to_string_lossy().into_owned()),
    }
}
fn version(root: &Path, path: &Path, bytes: &[u8]) -> Result<FileVersion> {
    Ok(FileVersion {
        path: relative(root, path)?,
        content_hash: hash(bytes),
        size: bytes.len() as u64,
    })
}
fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn line_delta(before: &[u8], after: &[u8]) -> (usize, usize) {
    crate::linediff::line_delta(before, after)
}

#[cfg(test)]
#[cfg(test)]
mod tests;
