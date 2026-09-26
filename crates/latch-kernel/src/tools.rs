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
use crate::execution::ExecutionBackend;
use crate::safety;
use crate::sandbox::{CapabilitySet, SandboxProfile};
use crate::store::EventStore;
use crate::tokens::TokenEstimator;
use anyhow::{Context, Result, anyhow, bail};
use latch_protocol::{
    ChangeOwner, EventPayload, FileVersion, MediaRef, Mode, PermissionMode, Safety, ToolCall,
    ToolResult,
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
    Ready(ExecutionBackend),
    Unavailable(String),
}

#[derive(Clone)]
pub struct ToolExecutor {
    workspace: PathBuf,
    artifacts: PathBuf,
    state_dir: PathBuf,
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
    /// Probed once at startup. `Some(message)` means the optional `rg` runtime
    /// dependency is unavailable, so `search` fails with that message instead
    /// of a bare `No such file or directory`.
    search_runtime: Arc<std::sync::RwLock<Option<String>>>,
}
/// Probes the optional `rg` runtime dependency once at startup. Returns the
/// actionable refusal message when ripgrep is missing or unusable.
fn probe_search_runtime() -> Option<String> {
    match std::process::Command::new("rg")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!(
            "ripgrep (`rg`) is required by the search tool, but `rg --version` failed with {status}"
        )),
        Err(error) => Some(format!(
            "ripgrep (`rg`) is required by the search tool but was not found on PATH ({error}); \
             install ripgrep and restart Latch"
        )),
    }
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
        Self::new_with_state_dir(
            workspace,
            artifacts,
            crate::config::Config::default().state_dir,
            store,
            session_id,
            policy,
        )
    }

    /// Production sessions pass their configured state directory explicitly.
    pub fn new_with_state_dir(
        workspace: PathBuf,
        artifacts: PathBuf,
        state_dir: PathBuf,
        store: EventStore,
        session_id: Uuid,
        policy: PolicyEngine,
    ) -> Result<Self> {
        std::fs::create_dir_all(&state_dir)?;
        std::fs::create_dir_all(&artifacts)?;
        let initial = git_dirty_hashes(&workspace).unwrap_or_default();
        let sandbox = match ExecutionBackend::detect(&workspace) {
            Ok(runner) => SandboxState::Ready(runner),
            Err(error) => SandboxState::Unavailable(format!("{error:#}")),
        };
        let search_runtime = probe_search_runtime();
        if let Some(message) = &search_runtime {
            tracing::warn!("{message}");
        }
        Ok(Self {
            workspace,
            artifacts,
            state_dir,
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
            search_runtime: Arc::new(std::sync::RwLock::new(search_runtime)),
        })
    }
    /// Creates a session-scoped executor for a child agent while retaining the
    /// root workspace coordinator. Policy locks are shared as a live capability
    /// ceiling; mutation/observation/ownership state is shared so concurrent
    /// agents cannot race the guarded workspace invariants. Process handles and
    /// one-shot grants remain isolated per child.
    pub fn for_child(&self, session_id: Uuid) -> Result<Self> {
        let artifacts = self.artifacts.parent().map_or_else(
            || self.artifacts.join(session_id.to_string()),
            |root| root.join(session_id.to_string()),
        );
        std::fs::create_dir_all(&artifacts)?;
        Ok(Self {
            workspace: self.workspace.clone(),
            artifacts,
            state_dir: self.state_dir.clone(),
            store: self.store.clone(),
            session_id,
            policy: self.policy.clone(),
            observations: self.observations.clone(),
            self_authored: self.self_authored.clone(),
            ledger: self.ledger.clone(),
            mutation_lock: self.mutation_lock.clone(),
            read_slots: self.read_slots.clone(),
            restored: self.restored.clone(),
            processes: Arc::new(Mutex::new(HashMap::new())),
            grants: Arc::new(std::sync::Mutex::new(HashMap::new())),
            sandbox: self.sandbox.clone(),
            search_runtime: self.search_runtime.clone(),
        })
    }
    /// Actionable message when the `rg` runtime dependency is unavailable;
    /// `None` means the search tool can run.
    pub fn search_runtime_error(&self) -> Option<String> {
        self.search_runtime
            .read()
            .map(|message| message.clone())
            .unwrap_or_else(|_| Some("search runtime state poisoned".into()))
    }
    #[cfg(test)]
    pub(crate) fn force_search_unavailable(&self, message: &str) {
        if let Ok(mut state) = self.search_runtime.write() {
            *state = Some(message.to_owned());
        }
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
            self.state_dir.clone(),
            capabilities,
        )
    }
    pub fn sandbox_runner_for_extension(&self) -> Result<ExecutionBackend> {
        self.sandbox_runner()
    }
    /// The sandbox is required, not best-effort: a failed probe refuses every
    /// command execution with the probe's actionable message.
    fn sandbox_runner(&self) -> Result<ExecutionBackend> {
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
        self.sandbox_runner().map(|runner| runner.status())
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
        let classification = self.classify_call(&call.name, &call.arguments);
        let mut capabilities = classification.capabilities;
        let mut external_roots = classification.external_roots;
        if let Some(grant) = self.grant_for(&call.id) {
            capabilities.extend(&grant.capabilities);
            external_roots.extend(grant.external_roots);
        }
        SandboxProfile::new(
            self.workspace.clone(),
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
            self.state_dir.clone(),
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
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.policy.mode()
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
        let classification = self.policy.classify(tool, args);
        let target = if matches!(tool, "write" | "patch") {
            args.get("path").and_then(Value::as_str).map(|raw| {
                if Path::new(raw).is_absolute() {
                    PathBuf::from(raw)
                } else {
                    self.workspace.join(raw)
                }
            })
        } else if matches!(tool, "shell" | "validate" | "exec_start") {
            args.get("command")
                .and_then(Value::as_str)
                .and_then(safety::inferred_write_target)
        } else {
            None
        };
        if target
            .as_ref()
            .is_some_and(|path| self.is_protected_path(path))
        {
            return safety::Classification::deny(
                classification.capabilities,
                classification.operation,
                "Latch protected state directory and credentials cannot be accessed by approval",
            );
        }
        classification
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
                "Read a bounded UTF-8 file window (default 400 lines). Small files include a hash. Supports 1-based offset, limit, tail; copy byte_offset and cursor_line from continuation when shown. Use read_image for images.",
                json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"},"tail":{"type":"integer"},"byte_offset":{"type":"integer"},"cursor_line":{"type":"integer"}}}),
            ),
            def(
                "read_image",
                "Inspect an image file (PNG, JPEG, or WebP) in the workspace. The image is ingested into Latch's immutable artifact store and attached to the conversation, so a vision-capable model sees the original pixels. Returns image metadata; do not use read_file for images.",
                json!({"type":"object","required":["path"],"properties":{"path":{"type":"string","description":"workspace-relative or workspace-contained image path"}}}),
            ),
            def(
                "search",
                "Search repository text with ripgrep. Returns a bounded page of matches (default 50) with total count and an offset continuation.",
                json!({"type":"object","required":["query"],"properties":{"query":{"type":"string"},"path":{"type":"string"},"max_results":{"type":"integer","description":"matches per page, default 50"},"offset":{"type":"integer","description":"0-based match offset for continuation"}}}),
            ),
            def(
                "read_artifact",
                "Read a bounded artifact window by id. Supports offset/limit/tail and byte_offset/cursor_line continuation.",
                json!({"type":"object","required":["id"],"properties":{"id":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"},"tail":{"type":"integer"},"byte_offset":{"type":"integer"},"cursor_line":{"type":"integer"}}}),
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
        let classification = self.classify_call(&call.name, &call.arguments);
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
            let reason = if matches!(call.name.as_str(), "read_file" | "read_artifact") {
                files::bound_final_output(&reason)
            } else {
                reason
            };
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
            "read_image" => self.read_image(call).await,
            other => match other {
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
            }
            .map(|(output, artifact_id)| (output, artifact_id, Vec::new())),
        };
        // The grant is single-use for exactly this call id.
        self.clear_grant(&call.id);
        let r = match outcome {
            Ok((output, artifact_id, media)) => {
                let output = if matches!(call.name.as_str(), "read_file" | "read_artifact") {
                    files::bound_final_output(&output)
                } else {
                    output
                };
                result_with_media(call, output, false, artifact_id, media)
            }
            Err(e) => {
                let output = format!("{e:#}");
                let output = if matches!(call.name.as_str(), "read_file" | "read_artifact") {
                    files::bound_final_output(&output)
                } else {
                    output
                };
                result(call, output, true, None)
            }
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
    /// The configured state directory resolved to its real filesystem
    /// location. Sandboxed commands are masked from it; kernel-native tools
    /// enforce the same boundary.
    fn protected_state_dir(&self) -> PathBuf {
        std::fs::canonicalize(&self.state_dir)
            .unwrap_or_else(|_| lexical_normalize(&self.state_dir))
    }
    /// True when `path` is the state directory or a descendant of it. Symlinks
    /// on the deepest existing ancestor are resolved first, so an alias into
    /// the state directory is protected even when the leaf does not exist yet.
    pub(super) fn is_protected_path(&self, path: &Path) -> bool {
        resolve_real_path(path).is_some_and(|real| real.starts_with(self.protected_state_dir()))
    }
    /// Refuses agent-visible filesystem access to the configured state
    /// directory and everything beneath it. Credentials and session state must
    /// never be readable through kernel-native tools just because the state
    /// directory happens to live inside the workspace.
    pub(super) fn ensure_not_protected(&self, path: &Path) -> Result<()> {
        if self.is_protected_path(path) {
            let display =
                relative(&self.workspace, path).unwrap_or_else(|_| path.display().to_string());
            bail!(
                "{display} is inside Latch's protected state directory; \
                 workspace tools cannot read or write it"
            );
        }
        Ok(())
    }
    /// Ripgrep `--glob` exclusion that keeps a recursive search from
    /// traversing the state directory when it lives inside the workspace.
    /// Patterns are matched relative to rg's cwd, which is the workspace.
    fn state_dir_exclusion_glob(&self) -> Option<String> {
        let workspace = std::fs::canonicalize(&self.workspace).ok()?;
        let relative = self
            .protected_state_dir()
            .strip_prefix(&workspace)
            .ok()?
            .to_string_lossy()
            .into_owned();
        if relative.is_empty() {
            return None;
        }
        Some(format!("!{}/**", escape_glob(&relative)))
    }
}

/// Resolves `path` to its real location, following symlinks on the deepest
/// existing ancestor and appending a nonexistent tail unchanged. Used by the
/// protected-path check so aliases and not-yet-created leaves resolve to the
/// same real path.
fn resolve_real_path(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path.to_path_buf();
    let mut tail = Vec::new();
    loop {
        if let Ok(mut real) = ancestor.canonicalize() {
            for component in tail.iter().rev() {
                real.push(component);
            }
            return Some(real);
        }
        tail.push(ancestor.file_name()?.to_os_string());
        if !ancestor.pop() {
            return None;
        }
    }
}

/// Escapes ripgrep glob metacharacters so a literal path is excluded exactly,
/// even when a directory name contains glob syntax.
fn escape_glob(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']' | '{' | '}') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
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
    result_with_media(call, output, is_error, artifact_id, Vec::new())
}

fn result_with_media(
    call: &ToolCall,
    output: String,
    is_error: bool,
    artifact_id: Option<String>,
    media: Vec<MediaRef>,
) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        output,
        is_error,
        artifact_id,
        media,
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
