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

/// Legacy decision mirror kept for existing callers; new code should use
/// [`crate::safety::Decision`] and the richer classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny(String),
    Ask(String),
}

impl From<safety::Decision> for PolicyDecision {
    fn from(decision: safety::Decision) -> Self {
        match decision {
            safety::Decision::Allow => Self::Allow,
            safety::Decision::Ask(reason) => Self::Ask(reason),
            safety::Decision::Deny(reason) => Self::Deny(reason),
        }
    }
}

/// Capabilities a resolver approved for exactly one call. Grants never
/// override a `Deny`; they only convert the matching `Ask` into an allow with
/// the narrowest capability set.
#[derive(Debug, Clone, Default)]
pub struct CapabilityGrant {
    pub capabilities: CapabilitySet,
    pub external_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct PolicyEngine {
    mode: Arc<RwLock<Mode>>,
    safety: Arc<RwLock<Safety>>,
    permissions: Arc<RwLock<PermissionMode>>,
    workspace: PathBuf,
    config: PermissionConfig,
}

impl PolicyEngine {
    #[must_use]
    pub fn new(mode: Mode, workspace: PathBuf, config: PermissionConfig) -> Self {
        Self {
            mode: Arc::new(RwLock::new(mode)),
            safety: Arc::new(RwLock::new(Safety::Standard)),
            permissions: Arc::new(RwLock::new(config.mode)),
            workspace,
            config,
        }
    }
    /// Constructor used by the CLI, which owns the full configuration and can
    /// supply the configured default safety profile.
    #[must_use]
    pub fn with_defaults(
        mode: Mode,
        workspace: PathBuf,
        config: PermissionConfig,
        safety: Safety,
    ) -> Self {
        let engine = Self::new(mode, workspace, config);
        engine.set_safety(safety);
        engine
    }
    pub fn set_mode(&self, mode: Mode) {
        if let Ok(mut current) = self.mode.write() {
            *current = mode;
        }
    }
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode.read().map_or(Mode::Ask, |mode| *mode)
    }
    pub fn set_safety(&self, safety: Safety) {
        if let Ok(mut current) = self.safety.write() {
            *current = safety;
        }
    }
    #[must_use]
    pub fn safety(&self) -> Safety {
        self.safety
            .read()
            .map_or(Safety::Standard, |safety| *safety)
    }
    pub fn set_permissions(&self, mode: PermissionMode) {
        if let Ok(mut current) = self.permissions.write() {
            *current = mode;
        }
    }
    #[must_use]
    pub fn permissions(&self) -> PermissionMode {
        self.permissions
            .read()
            .map_or(PermissionMode::Human, |mode| *mode)
    }
    /// Classifies one call through the safety layer.
    #[must_use]
    pub fn classify(&self, tool: &str, args: &Value) -> safety::Classification {
        safety::classify(
            tool,
            args,
            safety::Context {
                mode: self.mode(),
                safety: self.safety(),
                workspace: &self.workspace,
                outside: self.config.outside_workspace,
                workspace_write: self.config.workspace_write,
            },
        )
    }
    /// Decision only; retained for callers that do not need capabilities.
    #[must_use]
    pub fn decide(&self, tool: &str, args: &Value) -> PolicyDecision {
        match self.classify(tool, args).decision {
            safety::Decision::Allow => PolicyDecision::Allow,
            safety::Decision::Ask(reason) => PolicyDecision::Ask(reason),
            safety::Decision::Deny(reason) => PolicyDecision::Deny(reason),
        }
    }
}

#[derive(Debug, Clone)]
struct ChangeRecord {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after_hash: String,
    owner: ChangeOwner,
    /// Session artifact holding the pre-change bytes for resume-safe undo.
    undo_artifact: Option<String>,
}
#[derive(Debug, Default)]
struct ChangeLedger {
    initial: HashMap<PathBuf, String>,
    /// Chronological Latch- and shell-owned changes. `/undo` reverts the newest
    /// eligible entry.
    owned: Vec<ChangeRecord>,
    externally_changed: HashSet<PathBuf>,
    checkpoints: Vec<(Uuid, usize)>,
}

/// Bounded capture limits for shell-drift detection. Drift snapshots never
/// attempt to copy a whole repository: dirty-file reads and restored pre-content
/// are capped so large trees stay safe.
const DRIFT_MAX_PATHS: usize = 256;
const DRIFT_MAX_TRACKED: usize = 64;
const DRIFT_MAX_FILE_BYTES: u64 = 1024 * 1024;
const DRIFT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// `read_file` defaults to a bounded window with continuation. Files are never
/// silently injected whole into context; explicit limits may be larger than the
/// default but stay bounded per call.
const READ_DEFAULT_LINES: usize = 400;
const READ_MAX_LINES: usize = 20_000;
/// Secondary token cap for one read window. A 400-line window is already small,
/// but dense code can still be large; whole lines are trimmed until the
/// selection fits, and the continuation offset keeps the rest reachable.
const READ_MAX_TOKENS: usize = 8_000;
/// `search` returns a bounded page with an offset continuation.
const SEARCH_DEFAULT_RESULTS: usize = 50;
const SEARCH_MAX_RESULTS: usize = 500;
/// `read_artifact` defaults to the same window as a file read.
const ARTIFACT_DEFAULT_LINES: usize = 2_000;
const ARTIFACT_MAX_LINES: usize = 20_000;
/// Managed process output retained in memory before spilling to an artifact.
const PROCESS_MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Exited(i32),
    Killed,
}

impl ProcessStatus {
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Running => "running".into(),
            Self::Exited(code) => format!("exited with code {code}"),
            Self::Killed => "terminated".into(),
        }
    }
}

#[derive(Debug)]
struct ManagedProcess {
    label: String,
    status: ProcessStatus,
    child: Option<Child>,
    output: Arc<Mutex<String>>,
    readers: Vec<tokio::task::JoinHandle<()>>,
    /// Byte offset already returned to the model by `exec_poll`.
    cursor: usize,
    artifact_id: Option<String>,
}

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
            ledger: Arc::new(Mutex::new(ChangeLedger {
                initial,
                ..Default::default()
            })),
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
    pub async fn preexisting_change_count(&self) -> usize {
        self.ledger.lock().await.initial.len()
    }
    pub async fn latch_change_count(&self) -> usize {
        self.ledger.lock().await.owned.len()
    }
    /// Rebuilds durable change ownership from the event log after a resume.
    /// Entries whose post-change hash no longer matches the file stay in the
    /// ledger but `/undo` refuses them, exactly like an external edit in a live
    /// session.
    pub async fn restore_ownership(&self) -> Result<usize> {
        if self
            .restored
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(0);
        }
        let events = self.store.events(self.session_id)?;
        let mut restored = 0;
        let mut ledger = self.ledger.lock().await;
        for event in &events {
            match &event.payload {
                EventPayload::FileChanged {
                    after,
                    owner,
                    undo_artifact,
                    ..
                } if !matches!(owner, ChangeOwner::External) => {
                    let path = self.workspace.join(&after.path);
                    let record = ChangeRecord {
                        path,
                        before: None,
                        after_hash: after.content_hash.clone(),
                        owner: owner.clone(),
                        undo_artifact: undo_artifact.clone(),
                    };
                    // A change whose result is no longer on disk was externally
                    // modified after the fact; keep it distinguishable.
                    if tokio::fs::read(&record.path)
                        .await
                        .map(|bytes| hash(&bytes) != after.content_hash)
                        .unwrap_or(true)
                    {
                        ledger.externally_changed.insert(record.path.clone());
                    }
                    self.note_self_authored(&record.path, &after.content_hash);
                    ledger.owned.push(record);
                    restored += 1;
                }
                EventPayload::ChangeReverted { content_hash, path } => {
                    // The revert tombstone removes the original entry so a
                    // resumed ledger never re-applies undone work.
                    let absolute = self.workspace.join(path);
                    if let Some(position) = ledger.owned.iter().position(|record| {
                        record.path == absolute && record.after_hash == *content_hash
                    }) {
                        ledger.owned.remove(position);
                    }
                }
                _ => {}
            }
        }
        Ok(restored)
    }
    /// Records a content hash Latch itself produced, so later guarded edits
    /// can recognize their own drift instead of demanding a re-read.
    fn note_self_authored(&self, path: &Path, content_hash: &str) {
        if let Ok(mut authored) = self.self_authored.lock() {
            authored
                .entry(path.to_path_buf())
                .or_default()
                .insert(content_hash.to_owned());
        }
    }

    fn is_self_authored(&self, path: &Path, content_hash: &str) -> bool {
        self.self_authored
            .lock()
            .map(|authored| {
                authored
                    .get(path)
                    .is_some_and(|hashes| hashes.contains(content_hash))
            })
            .unwrap_or(false)
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
    async fn read_file(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _permit = self.read_slots.acquire().await?;
        let path = self.path_arg(call)?;
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        let version = version(&self.workspace, &path, &bytes)?;
        let previous = self
            .observations
            .lock()
            .await
            .insert(path.clone(), version.clone());
        if let Some(previous) = previous
            && previous.content_hash != version.content_hash
            && !self
                .ledger
                .lock()
                .await
                .owned
                .iter()
                .any(|change| change.path == path && change.after_hash == version.content_hash)
        {
            self.store.append(
                self.session_id,
                EventPayload::ExternalFileChangeDetected {
                    path: version.path.clone(),
                    expected_hash: previous.content_hash,
                    actual_hash: version.content_hash.clone(),
                },
            )?;
            self.ledger.lock().await.externally_changed.insert(path);
        }
        self.store.append(
            self.session_id,
            EventPayload::FileObserved {
                version: version.clone(),
            },
        )?;
        let text = String::from_utf8(bytes).context("file is not UTF-8")?;
        let total = text.lines().count();
        let mut window = LineWindow::from_args(call, total, READ_DEFAULT_LINES, READ_MAX_LINES)?;
        let estimator = TokenEstimator::generic();
        let token_bounded = window.token_bound(&text, READ_MAX_TOKENS, &estimator);
        let selected = window.slice(&text);
        let mut out = format!("hash: {}\n", version.content_hash);
        out.push_str(&format!("[{}: {}]\n", version.path, window.describe(total)));
        out.push_str(&selected);
        if let Some(offset) = window.continue_offset(total) {
            if token_bounded {
                out.push_str(&format!(
                    "\n[token-bounded window; continue with offset={offset}]"
                ));
            } else {
                out.push_str(&format!("\n[continue with offset={offset}]"));
            }
        }
        Ok((out, None))
    }
    async fn search(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _permit = self.read_slots.acquire().await?;
        let q = str_arg(call, "query")?;
        let target = call
            .arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".");
        let path = resolve_workspace_path(&self.workspace, target)?;
        let max_results = call
            .arguments
            .get("max_results")
            .and_then(Value::as_u64)
            .map_or(SEARCH_DEFAULT_RESULTS, |value| {
                (value as usize).clamp(1, SEARCH_MAX_RESULTS)
            });
        let offset = call
            .arguments
            .get("offset")
            .and_then(Value::as_u64)
            .map_or(0, |value| value as usize);
        let out = Command::new("rg")
            .args([
                "-n",
                "--color=never",
                "--no-heading",
                "--max-columns",
                "400",
                "--max-columns-preview",
                "--max-filesize",
                "8M",
                "-m",
                "1000",
                "--",
                q,
            ])
            .arg(path)
            .current_dir(&self.workspace)
            .output()
            .await?;
        if !out.status.success() && out.status.code() != Some(1) {
            bail!(
                "search failed with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let matches = text.lines().collect::<Vec<_>>();
        let total = matches.len();
        if offset >= total {
            return Ok((
                format!("{total} match(es); offset {offset} is past the end"),
                None,
            ));
        }
        let end = (offset + max_results).min(total);
        let mut result = format!(
            "{total} match(es); showing {}-{} of {total}\n{}",
            offset + 1,
            end,
            matches[offset..end].join("\n")
        );
        if end < total {
            result.push_str(&format!("\n[continue with offset={end}]"));
        }
        Ok((result, None))
    }
    async fn read_artifact(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _permit = self.read_slots.acquire().await?;
        let id = str_arg(call, "id")?;
        let path = self.artifact_path(id)?;
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("read artifact {id}"))?;
        let text = String::from_utf8_lossy(&bytes);
        let window = LineWindow::from_args(
            call,
            text.lines().count(),
            ARTIFACT_DEFAULT_LINES,
            ARTIFACT_MAX_LINES,
        )?;
        let mut out = format!(
            "[artifact {id}: {}]\n",
            window.describe(text.lines().count())
        );
        out.push_str(&window.slice(&text));
        if let Some(offset) = window.continue_offset(text.lines().count()) {
            out.push_str(&format!("\n[continue with offset={offset}]"));
        }
        Ok((out, None))
    }
    /// Starts a persistent development process owned by the kernel. Output is
    /// buffered for `exec_poll`; lifecycle is durable so a resumed session can
    /// report honestly that the child did not survive the restart.
    async fn process_start(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let command = str_arg(call, "command")?.to_owned();
        let label = call
            .arguments
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let id = format!("proc-{}", Uuid::new_v4());
        let profile = self.sandbox_profile(call);
        let runner = self.sandbox_runner()?;
        let mut child = runner
            .command(&profile, &command)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start process: {command}"))?;
        let pid = child.id();
        let output = Arc::new(Mutex::new(String::new()));
        let mut readers = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            readers.push(spawn_reader(stdout, output.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.push(spawn_reader(stderr, output.clone()));
        }
        self.store.append(
            self.session_id,
            EventPayload::ProcessStarted {
                id: id.clone(),
                command: command.clone(),
                label: label.clone(),
                pid,
            },
        )?;
        self.processes.lock().await.insert(
            id.clone(),
            ManagedProcess {
                label: label.clone(),
                status: ProcessStatus::Running,
                child: Some(child),
                output,
                readers,
                cursor: 0,
                artifact_id: None,
            },
        );
        let label_suffix = if label.is_empty() {
            String::new()
        } else {
            format!(" ({label})")
        };
        Ok((
            format!("process {id} started{label_suffix}\n[exec_poll id={id}]"),
            None,
        ))
    }
    async fn process_poll(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let id = str_arg(call, "id")?;
        let mut processes = self.processes.lock().await;
        let Some(process) = processes.get_mut(id) else {
            if self.process_was_started(id)? {
                bail!(
                    "process {id} is no longer available: child processes do not survive a Latch restart"
                );
            }
            bail!("unknown process {id}; start one with exec_start");
        };
        if process.status == ProcessStatus::Running
            && let Some(child) = process.child.as_mut()
            && let Some(status) = child.try_wait()?
        {
            process.status = ProcessStatus::Exited(status.code().unwrap_or(-1));
            process.child = None;
            for handle in process.readers.drain(..) {
                let _ = handle.await;
            }
            self.finish_process(id, process).await?;
        }
        let full = process.output.lock().await.clone();
        let new = full.get(process.cursor..).unwrap_or("").to_owned();
        process.cursor = full.len();
        let label = if process.label.is_empty() {
            String::new()
        } else {
            format!(" {}", process.label)
        };
        let mut text = format!("[{id}{label}: {}]\n", process.status.label());
        if new.is_empty() {
            text.push_str("(no new output)");
        } else {
            text.push_str(&new);
        }
        if let Some(artifact) = &process.artifact_id {
            text.push_str(&format!("\n[full output artifact: {artifact}]"));
        }
        Ok((text, process.artifact_id.clone()))
    }
    async fn process_terminate(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let id = str_arg(call, "id")?;
        let mut processes = self.processes.lock().await;
        let Some(process) = processes.get_mut(id) else {
            if self.process_was_started(id)? {
                bail!("process {id} is not running; it did not survive a Latch restart");
            }
            bail!("unknown process {id}");
        };
        if process.status == ProcessStatus::Running {
            if let Some(child) = process.child.as_mut() {
                child.kill().await.ok();
                let _ = child.wait().await;
            }
            process.status = ProcessStatus::Killed;
            process.child = None;
            for handle in process.readers.drain(..) {
                let _ = handle.await;
            }
            self.finish_process(id, process).await?;
        }
        Ok((
            format!("[process {id}: {}]", process.status.label()),
            process.artifact_id.clone(),
        ))
    }
    async fn finish_process(&self, id: &str, process: &mut ManagedProcess) -> Result<()> {
        let full = process.output.lock().await.clone();
        let artifact_id = if full.len() > PROCESS_MAX_BUFFER_BYTES {
            let name = format!("process-{id}.log");
            std::fs::write(self.artifacts.join(&name), full.as_bytes())?;
            process.artifact_id = Some(name.clone());
            Some(name)
        } else {
            process.artifact_id.clone()
        };
        let status = match process.status {
            ProcessStatus::Running => "lost".into(),
            ProcessStatus::Exited(code) => format!("exit {code}"),
            ProcessStatus::Killed => "killed".into(),
        };
        self.store.append(
            self.session_id,
            EventPayload::ProcessExited {
                id: id.to_owned(),
                status,
                artifact_id,
            },
        )?;
        Ok(())
    }
    fn process_was_started(&self, id: &str) -> Result<bool> {
        Ok(self
            .store
            .events(self.session_id)?
            .iter()
            .any(|event| matches!(&event.payload, EventPayload::ProcessStarted { id: started, .. } if started == id)))
    }
    fn artifact_path(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty()
            || id.contains("..")
            || id.contains('/')
            || id.contains('\\')
            || Path::new(id).is_absolute()
        {
            bail!("invalid artifact id");
        }
        let path = self.artifacts.join(id);
        let canonical = path
            .canonicalize()
            .with_context(|| format!("unknown artifact {id}"))?;
        let root = self.artifacts.canonicalize()?;
        if !canonical.starts_with(&root) {
            bail!("artifact path escapes the artifact store");
        }
        Ok(canonical)
    }
    async fn patch(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _guard = self.mutation_lock.lock().await;
        let path = self.write_path_arg(call)?;
        let base = str_arg(call, "base_hash")?;
        let old = str_arg(call, "old")?;
        let new = str_arg(call, "new")?;
        let before = tokio::fs::read(&path).await?;
        self.ensure_fresh(&path, &before, base).await?;
        let text = String::from_utf8(before.clone())?;
        let occurrences = text.matches(old).count();
        if occurrences != 1 {
            bail!("expected exactly one match, found {occurrences}");
        }
        let updated = text.replacen(old, new, 1).into_bytes();
        self.commit_change(
            path,
            Some(before),
            updated,
            ChangeOwner::Latch,
            Some(&call.id),
        )
        .await
    }
    async fn write(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _guard = self.mutation_lock.lock().await;
        let path = self.write_path_arg(call)?;
        let content = str_arg(call, "content")?.as_bytes().to_vec();
        let before = tokio::fs::read(&path).await.ok();
        match (
            &before,
            call.arguments.get("base_hash").and_then(Value::as_str),
        ) {
            (Some(bytes), Some(base)) => self.ensure_fresh(&path, bytes, base).await?,
            (Some(_), None) => bail!("existing file requires base_hash from read_file"),
            (None, Some(_)) => bail!("new file must not provide base_hash"),
            (None, None) => {}
        }
        self.commit_change(path, before, content, ChangeOwner::Latch, Some(&call.id))
            .await
    }
    async fn ensure_fresh(&self, path: &Path, bytes: &[u8], base: &str) -> Result<()> {
        let actual = hash(bytes);
        // Drift onto a hash Latch itself wrote is self-authored: the guarded
        // edit proceeds against current content without a forced re-read.
        // Genuine external modification still fails below.
        if actual != base && !self.is_self_authored(path, &actual) {
            self.store.append(
                self.session_id,
                EventPayload::ExternalFileChangeDetected {
                    path: relative(&self.workspace, path)?,
                    expected_hash: base.into(),
                    actual_hash: actual.clone(),
                },
            )?;
            self.ledger
                .lock()
                .await
                .externally_changed
                .insert(path.to_path_buf());
            bail!("stale observation: expected {base}, found {actual}; re-read before editing");
        }
        Ok(())
    }
    async fn commit_change(
        &self,
        path: PathBuf,
        before: Option<Vec<u8>>,
        after: Vec<u8>,
        owner: ChangeOwner,
        call_id: Option<&str>,
    ) -> Result<(String, Option<String>)> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let before_version = before
            .as_ref()
            .map(|b| version(&self.workspace, &path, b))
            .transpose()?;
        let undo_artifact = before
            .as_ref()
            .map(|bytes| self.store_undo_artifact(bytes))
            .transpose()?;
        let operation = self.store.begin_operation(
            self.session_id,
            &format!("guarded write: {}", path.display()),
        )?;
        if let Err(error) = tokio::fs::write(&path, &after).await {
            self.store.finish_operation(operation)?;
            return Err(error.into());
        }
        let after_version = version(&self.workspace, &path, &after)?;
        self.observations
            .lock()
            .await
            .insert(path.clone(), after_version.clone());
        self.note_self_authored(&path, &after_version.content_hash);
        let (additions, deletions) = line_delta(before.as_deref().unwrap_or_default(), &after);
        let preview = relative(&self.workspace, &path)
            .map(|relative_path| {
                crate::linediff::unified_diff(
                    &relative_path,
                    before.as_deref(),
                    Some(&after),
                    DIFF_PREVIEW_LINES,
                )
            })
            .unwrap_or_default();
        self.ledger.lock().await.owned.push(ChangeRecord {
            path: path.clone(),
            before,
            after_hash: after_version.content_hash.clone(),
            owner: owner.clone(),
            undo_artifact: undo_artifact.clone(),
        });
        self.store.append(
            self.session_id,
            EventPayload::FileChanged {
                before: before_version,
                after: after_version.clone(),
                owner,
                undo_artifact,
                additions,
                deletions,
                preview,
                call_id: call_id.map(str::to_owned),
            },
        )?;
        self.store.finish_operation(operation)?;
        Ok((
            format!(
                "updated {} @ {}",
                after_version.path, after_version.content_hash
            ),
            None,
        ))
    }
    /// Content-addressed artifact holding pre-change bytes so ownership and
    /// undo survive process restart without storing file bodies in event JSON.
    fn store_undo_artifact(&self, bytes: &[u8]) -> Result<String> {
        let name = format!("changes/{}.before", hash(bytes));
        let path = self.artifacts.join(&name);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)?;
        }
        Ok(name)
    }
    /// Runs a bounded shell command with workspace-drift classification, shared
    /// by the `shell` tool and kernel validation. Drift detection is skipped
    /// only for commands conservatively classified as read-only.
    pub async fn run_process(
        &self,
        profile: &SandboxProfile,
        command: &str,
        timeout_seconds: u64,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput> {
        let drift = !is_read_only_shell(command, &self.workspace);
        let before = if drift {
            self.snapshot_dirty().await.ok().flatten()
        } else {
            None
        };
        let started = Instant::now();
        let (status, text, artifact) = self
            .run_process_inner(profile, command, timeout_seconds, cancel)
            .await?;
        if drift {
            self.classify_drift(command, before.as_deref()).await;
        }
        Ok(ProcessOutput {
            success: status.success(),
            status_line: format!("exit code {}", status.code().unwrap_or(-1)),
            text,
            artifact_id: artifact,
            elapsed: started.elapsed(),
        })
    }
    async fn run_process_inner(
        &self,
        profile: &SandboxProfile,
        command: &str,
        timeout_seconds: u64,
        cancel: CancellationToken,
    ) -> Result<(std::process::ExitStatus, String, Option<String>)> {
        let op = self
            .store
            .begin_operation(self.session_id, &format!("shell: {command}"))?;
        let runner = self.sandbox_runner()?;
        let mut child = runner
            .command(profile, command)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("missing stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("missing stderr"))?;
        let stdout_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let stderr_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let waited = tokio::select! {
            () = cancel.cancelled() => None,
            result = tokio::time::timeout(Duration::from_secs(timeout_seconds), child.wait()) => Some(result),
        };
        let status = match waited {
            Some(Ok(result)) => result?,
            Some(Err(_)) => {
                child.kill().await.ok();
                child.wait().await.ok();
                self.store.finish_operation(op)?;
                bail!("shell command timed out");
            }
            None => {
                child.kill().await.ok();
                child.wait().await.ok();
                self.store.finish_operation(op)?;
                bail!("cancelled");
            }
        };
        let stdout = stdout_task.await??;
        let stderr = stderr_task.await??;
        self.store.finish_operation(op)?;
        let mut text = String::from_utf8_lossy(&stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&stderr));
        let (bounded, artifact) = self.bound_output(text, "shell")?;
        Ok((status, bounded, artifact))
    }
    /// Runs a `validate` command inside the same sandbox as `shell`.
    pub async fn run_validated_command(
        &self,
        call: &ToolCall,
        timeout_seconds: u64,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput> {
        let command = call
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("command is required"))?
            .to_owned();
        let profile = self.sandbox_profile(call);
        self.run_process(&profile, &command, timeout_seconds, cancel)
            .await
    }
    async fn shell(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
    ) -> Result<(String, Option<String>)> {
        let command = str_arg(call, "command")?;
        let timeout = call
            .arguments
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(self.policy.config.shell_timeout_seconds);
        let profile = self.sandbox_profile(call);
        let output = self.run_process(&profile, command, timeout, cancel).await?;
        if !output.success {
            bail!("{}\n{}", output.status_line, output.text)
        }
        Ok((format!("exit 0\n{}", output.text), output.artifact_id))
    }
    /// Captures the pre-command state of dirty and untracked files for drift
    /// classification. `Ok(None)` means the workspace is not a Git worktree and
    /// honest drift detection is unavailable.
    async fn snapshot_dirty(&self) -> Result<Option<Vec<(PathBuf, Vec<u8>)>>> {
        let listing = git_porcelain(&self.workspace)?;
        let Some(listing) = listing else {
            return Ok(None);
        };
        let mut captured = Vec::new();
        let mut total = 0usize;
        for path in listing.iter().take(DRIFT_MAX_PATHS) {
            let Ok(meta) = tokio::fs::metadata(path).await else {
                continue;
            };
            if !meta.is_file() || meta.len() > DRIFT_MAX_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = tokio::fs::read(path).await else {
                continue;
            };
            total += bytes.len();
            if total > DRIFT_MAX_TOTAL_BYTES {
                break;
            }
            captured.push((path.clone(), bytes));
        }
        Ok(Some(captured))
    }
    /// Classifies paths a shell command mutated as shell-originated change.
    ///
    /// Git workspaces: pre-change content comes from the captured dirty-file
    /// snapshot, or from `HEAD` for files that were clean (and therefore
    /// identical to `HEAD`) before the command. Reversible paths become
    /// undoable ledger entries owned by `Shell`; anything Latch could not
    /// capture is recorded as an explicitly non-reversible mutation. Non-Git
    /// workspaces get an honest "detection unavailable" marker instead of a
    /// pretense that nothing changed.
    async fn classify_drift(&self, command: &str, before: Option<&[(PathBuf, Vec<u8>)]>) {
        let mark_unavailable = |executor: &Self| {
            let _ = executor.store.append(
                executor.session_id,
                EventPayload::ShellMutationObserved {
                    command: command.into(),
                    reversible: false,
                    paths: vec![],
                },
            );
        };
        let Some(before) = before else {
            mark_unavailable(self);
            return;
        };
        let Ok(listing) = git_porcelain(&self.workspace) else {
            mark_unavailable(self);
            return;
        };
        let listing = listing.unwrap_or_default();
        let before_paths: HashSet<&Path> = before.iter().map(|(path, _)| path.as_path()).collect();
        let after_paths: HashSet<&Path> = listing.iter().map(|path| path.as_path()).collect();
        let mut candidates: Vec<PathBuf> = listing
            .iter()
            .filter(|path| !before_paths.contains(path.as_path()))
            .cloned()
            .collect();
        candidates.extend(
            before
                .iter()
                .filter(|(path, _)| !after_paths.contains(path.as_path()))
                .map(|(path, _)| path.clone()),
        );
        // Files that were already dirty before the command count as changed
        // when their captured content no longer matches disk.
        for (path, bytes) in before {
            if after_paths.contains(path.as_path())
                && before_paths.contains(path.as_path())
                && tokio::fs::read(path)
                    .await
                    .map(|current| hash(&current) != hash(bytes))
                    .unwrap_or(true)
            {
                candidates.push(path.clone());
            }
        }
        candidates.sort();
        candidates.dedup();
        let mut tracked = 0;
        let mut untrackable: Vec<String> = Vec::new();
        for path in candidates {
            let display =
                || relative(&self.workspace, &path).unwrap_or_else(|_| path.display().to_string());
            if tracked >= DRIFT_MAX_TRACKED {
                untrackable.push(display());
                continue;
            }
            let current = tokio::fs::read(&path).await.ok();
            let before_bytes = match before.iter().find(|(candidate, _)| **candidate == path) {
                Some((_, bytes)) => Some(bytes.clone()),
                None => {
                    // The file was clean before the command: its worktree
                    // content was exactly the HEAD version. A path with no
                    // HEAD blob that now exists was created by the command and
                    // is reversible by deletion (before = None).
                    match git_show_head(&self.workspace, &path) {
                        Some(bytes) => Some(bytes),
                        None if current.is_some() => None,
                        None => {
                            untrackable.push(display());
                            continue;
                        }
                    }
                }
            };
            let changed = match (&before_bytes, &current) {
                (Some(before), Some(current)) => hash(before) != hash(current),
                (Some(_), None) | (None, Some(_)) => true,
                (None, None) => false,
            };
            if !changed {
                continue;
            }
            let Some(after_bytes) = current else {
                // The command deleted a workspace path. Deletion has no
                // post-change file version to record, so it is reported
                // honestly as a non-reversible mutation.
                untrackable.push(display());
                continue;
            };
            tracked += 1;
            let Ok(after) = version(&self.workspace, &path, &after_bytes) else {
                untrackable.push(display());
                continue;
            };
            let before_version = before_bytes
                .as_ref()
                .map(|bytes| version(&self.workspace, &path, bytes))
                .transpose()
                .ok()
                .flatten();
            let undo_artifact = before_bytes
                .as_ref()
                .and_then(|bytes| self.store_undo_artifact(bytes).ok());
            let (additions, deletions) =
                line_delta(before_bytes.as_deref().unwrap_or_default(), &after_bytes);
            self.note_self_authored(&path, &after.content_hash);
            let preview = relative(&self.workspace, &path)
                .map(|relative_path| {
                    crate::linediff::unified_diff(
                        &relative_path,
                        before_bytes.as_deref(),
                        Some(&after_bytes),
                        DIFF_PREVIEW_LINES,
                    )
                })
                .unwrap_or_default();
            self.ledger.lock().await.owned.push(ChangeRecord {
                path: path.clone(),
                before: before_bytes,
                after_hash: after.content_hash.clone(),
                owner: ChangeOwner::Shell,
                undo_artifact: undo_artifact.clone(),
            });
            let _ = self.store.append(
                self.session_id,
                EventPayload::FileChanged {
                    before: before_version,
                    after,
                    owner: ChangeOwner::Shell,
                    undo_artifact,
                    additions,
                    deletions,
                    preview,
                    call_id: None,
                },
            );
        }
        if !untrackable.is_empty() {
            let _ = self.store.append(
                self.session_id,
                EventPayload::ShellMutationObserved {
                    command: command.into(),
                    reversible: false,
                    paths: untrackable,
                },
            );
        }
    }
    async fn git_status(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let profile = self.sandbox_profile(call);
        let runner = self.sandbox_runner()?;
        let output = runner
            .command(&profile, "git status --short --branch; git diff --stat")
            .output()
            .await?;
        if !output.status.success() {
            bail!(
                "git status failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok((String::from_utf8_lossy(&output.stdout).into_owned(), None))
    }
    async fn git_diff(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let profile = self.sandbox_profile(call);
        let runner = self.sandbox_runner()?;
        let output = runner
            .command(&profile, "git diff --no-ext-diff --")
            .output()
            .await?;
        if !output.status.success() {
            bail!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        self.bound_output(String::from_utf8_lossy(&output.stdout).into_owned(), "diff")
    }
    async fn checkpoint(&self, _: &ToolCall) -> Result<(String, Option<String>)> {
        let mut l = self.ledger.lock().await;
        let id = Uuid::new_v4();
        let position = l.owned.len();
        l.checkpoints.push((id, position));
        self.store.append(
            self.session_id,
            EventPayload::CheckpointCreated {
                id,
                label: "manual".into(),
            },
        )?;
        Ok((id.to_string(), None))
    }
    async fn undo(&self, _: &ToolCall) -> Result<(String, Option<String>)> {
        let _guard = self.mutation_lock.lock().await;
        // Peek before acting: a refused undo must not silently drop the change
        // from the ledger.
        let change = self
            .ledger
            .lock()
            .await
            .owned
            .last()
            .cloned()
            .ok_or_else(|| anyhow!("no Latch-owned change to undo"))?;
        let current = tokio::fs::read(&change.path).await.ok();
        if current.as_deref().map(hash).as_deref() != Some(&change.after_hash) {
            bail!(
                "cannot undo: {} changed since the {} edit",
                relative(&self.workspace, &change.path)
                    .unwrap_or_else(|_| change.path.display().to_string()),
                match change.owner {
                    ChangeOwner::Shell => "shell",
                    _ => "Latch",
                }
            );
        }
        let before = match &change.before {
            Some(bytes) => Some(bytes.clone()),
            None => match &change.undo_artifact {
                Some(artifact) => Some(
                    tokio::fs::read(self.artifacts.join(artifact))
                        .await
                        .with_context(|| format!("read undo artifact {artifact}"))?,
                ),
                None => None,
            },
        };
        match before {
            Some(bytes) => tokio::fs::write(&change.path, bytes).await?,
            None => tokio::fs::remove_file(&change.path).await?,
        }
        let relative_path = relative(&self.workspace, &change.path)?;
        // The bytes restored (or removed) are Latch-authored too.
        if let Ok(current) = tokio::fs::read(&change.path).await {
            self.note_self_authored(&change.path, &hash(&current));
        }
        self.ledger.lock().await.owned.pop();
        // Tombstone so a resumed session never restores the undone entry, plus
        // the durable audit record of the revert itself.
        self.store.append(
            self.session_id,
            EventPayload::ChangeReverted {
                path: relative_path.clone(),
                content_hash: change.after_hash,
            },
        )?;
        Ok((format!("undid {relative_path}"), None))
    }
    fn bound_output(&self, text: String, prefix: &str) -> Result<(String, Option<String>)> {
        const LIMIT: usize = 24_000;
        if text.len() <= LIMIT {
            return Ok((text, None));
        }
        let id = format!("{}-{}.log", prefix, Uuid::new_v4());
        std::fs::write(self.artifacts.join(&id), text.as_bytes())?;
        let mut end = LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Ok((
            format!("{}\n[truncated; full artifact: {id}]", &text[..end]),
            Some(id),
        ))
    }
    fn path_arg(&self, call: &ToolCall) -> Result<PathBuf> {
        resolve_workspace_path(&self.workspace, str_arg(call, "path")?)
    }

    /// Write paths honor an explicit outside-workspace approval: the user saw
    /// the exact call and approved it, so the write may target an absolute path
    /// outside the workspace. Without approval the normal containment rules
    /// apply and the call never reaches this point.
    fn write_path_arg(&self, call: &ToolCall) -> Result<PathBuf> {
        let raw = str_arg(call, "path")?;
        let candidate = if Path::new(raw).is_absolute() {
            lexical_normalize(Path::new(raw))
        } else {
            lexical_normalize(&self.workspace.join(raw))
        };
        if resolve_workspace_path(&self.workspace, raw).is_ok() {
            return Ok(candidate);
        }
        // Outside the workspace: only a scoped, single-use grant allows it.
        if self.grant_allows_path(&call.id, &candidate) {
            return Ok(candidate);
        }
        bail!("path escapes workspace and no capability grant covers it")
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
/// A bounded line window with head, offset/limit, and tail modes. Used by
/// `read_file` and `read_artifact` so no tool ever injects a whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineWindow {
    start: usize,
    end: usize,
}

impl LineWindow {
    fn from_args(
        call: &ToolCall,
        total: usize,
        default_lines: usize,
        max_lines: usize,
    ) -> Result<Self> {
        if let Some(tail) = call.arguments.get("tail").and_then(Value::as_u64) {
            let tail = (tail as usize).clamp(1, max_lines);
            return Ok(Self {
                start: total.saturating_sub(tail),
                end: total,
            });
        }
        let offset = call
            .arguments
            .get("offset")
            .and_then(Value::as_u64)
            .map_or(0, |value| value.saturating_sub(1) as usize);
        let limit = call
            .arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(default_lines, |value| value as usize)
            .clamp(1, max_lines);
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        Ok(Self { start, end })
    }

    /// Trims the window to whole lines that fit `max_tokens`, returning true
    /// when lines were dropped. The continuation offset then points at the
    /// first dropped line.
    fn token_bound(&mut self, text: &str, max_tokens: usize, estimator: &TokenEstimator) -> bool {
        let lines: Vec<&str> = text
            .lines()
            .skip(self.start)
            .take(self.end.saturating_sub(self.start))
            .collect();
        let mut used = 0usize;
        let mut keep = 0usize;
        for (index, line) in lines.iter().enumerate() {
            let cost = estimator.estimate(line).saturating_add(1);
            if index > 0 && used + cost > max_tokens {
                break;
            }
            used += cost;
            keep = index + 1;
        }
        if keep < lines.len() {
            self.end = self.start + keep;
            true
        } else {
            false
        }
    }

    fn slice(self, text: &str) -> String {
        text.lines()
            .skip(self.start)
            .take(self.end.saturating_sub(self.start))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn describe(self, total: usize) -> String {
        if total == 0 {
            return "empty".into();
        }
        if self.start >= total {
            return format!("no lines (offset past end of {total})");
        }
        format!("lines {}-{} of {total}", self.start + 1, self.end)
    }

    fn continue_offset(self, total: usize) -> Option<usize> {
        (self.end < total).then_some(self.end + 1)
    }
}

fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    output: Arc<Mutex<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => output
                    .lock()
                    .await
                    .push_str(&String::from_utf8_lossy(&buffer[..n])),
            }
        }
    })
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
/// Upper bound on the unified-diff preview stored with a change. The full
/// workspace diff remains available through `git_diff` and `/diff`.
const DIFF_PREVIEW_LINES: usize = 160;
/// Conservatively classifies a shell command as read-only.
///
/// Only simple inspection commands and compound commands built exclusively
/// from them are accepted. Any shell feature that could expand, substitute,
/// redirect, background, or nest is rejected, as is `||`. A `cd` into a
/// workspace-local subdirectory is accepted for compound read-only inspection,
/// but the target is resolved and normalized against the workspace root and
/// may never escape through `..`, an absolute path, or a symlink. This
/// deliberately keeps read-only classification stricter than the shell's
/// actual grammar so ASK/PLAN can never be used to mutate the workspace.
pub(crate) fn is_read_only_shell(command: &str, workspace: &Path) -> bool {
    let command = command.trim();
    if command.is_empty() || command.contains("||") {
        return false;
    }
    // `&&` is the only context where `&` is permitted; reject it everywhere
    // else (background jobs) along with expansion and redirection operators.
    let without_and = command.replace("&&", " ");
    if without_and.chars().any(|ch| {
        matches!(
            ch,
            '$' | '`'
                | '<'
                | '>'
                | '&'
                | '('
                | ')'
                | '\\'
                | '\n'
                | '\r'
                | '"'
                | '\''
                | '*'
                | '?'
                | '['
                | ']'
                | '{'
                | '}'
                | '~'
                | '!'
        )
    }) {
        return false;
    }
    let Some(segments) = split_simple_commands(command) else {
        return false;
    };
    if !segments
        .iter()
        .any(|segment| segment.split_whitespace().next() == Some("cd"))
    {
        // No `cd`: workspace is irrelevant, keep the fast conservative path.
        return segments.iter().all(|segment| is_read_only_command(segment));
    }
    // Workspace-aware: thread a virtual cwd through the chain starting at the
    // workspace root. Every `cd` target must resolve to a path that stays
    // inside the workspace, both lexically and canonically.
    let Some(root) = workspace.canonicalize().ok() else {
        return false;
    };
    let mut cwd = root.clone();
    for segment in segments {
        let mut words = segment.split_whitespace();
        let Some(first) = words.next() else {
            return false;
        };
        if first == "cd" {
            let args = words.collect::<Vec<_>>();
            if args.len() != 1 {
                return false;
            }
            let target = cwd.join(args[0]);
            let Ok(resolved) = resolve_workspace_path(&root, &target.to_string_lossy()) else {
                return false;
            };
            cwd = resolved;
        } else if !is_read_only_command(segment) {
            return false;
        }
    }
    true
}

fn split_simple_commands(command: &str) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut start = 0;
    let bytes = command.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let separator = match bytes[index] {
            b'&' if bytes.get(index + 1) == Some(&b'&') => 2,
            b'|' | b';' => 1,
            _ => {
                index += 1;
                continue;
            }
        };
        let segment = command[start..index].trim();
        if segment.is_empty() {
            return None;
        }
        segments.push(segment);
        index += separator;
        start = index;
    }
    let segment = command[start..].trim();
    if segment.is_empty() {
        return None;
    }
    segments.push(segment);
    Some(segments)
}

fn is_read_only_command(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    let args = words.collect::<Vec<_>>();
    match first {
        "rg" => !args.iter().any(|arg| arg.starts_with("--pre")),
        "grep" | "ls" | "pwd" | "head" | "tail" | "wc" | "cat" => true,
        "find" => !args.iter().any(|arg| {
            matches!(
                *arg,
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fls"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
            )
        }),
        "git" => {
            if args
                .iter()
                .any(|arg| *arg == "--output" || arg.starts_with("--output="))
            {
                return false;
            }
            match args.first().copied() {
                Some(
                    "status" | "diff" | "log" | "show" | "rev-parse" | "ls-files" | "describe"
                    | "blame" | "shortlog",
                ) => true,
                Some("branch") => args[1..].iter().all(|arg| {
                    matches!(
                        *arg,
                        "--list"
                            | "--all"
                            | "--remotes"
                            | "-a"
                            | "-r"
                            | "-v"
                            | "-vv"
                            | "--show-current"
                    )
                }),
                Some("remote") => args[1..]
                    .iter()
                    .all(|arg| matches!(*arg, "-v" | "--verbose" | "show")),
                _ => false,
            }
        }
        _ => false,
    }
}
fn git_dirty_hashes(workspace: &Path) -> Result<HashMap<PathBuf, String>> {
    let Some(paths) = git_porcelain(workspace)? else {
        return Ok(HashMap::new());
    };
    let mut map = HashMap::new();
    for path in paths.iter().take(DRIFT_MAX_PATHS) {
        if let Ok(bytes) = std::fs::read(path) {
            map.insert(path.clone(), hash(&bytes));
        }
    }
    Ok(map)
}
/// `git status --porcelain -z --untracked-files=all` parsed into workspace
/// paths, or `None` when the workspace is not inside a Git worktree.
fn git_porcelain(workspace: &Path) -> Result<Option<Vec<PathBuf>>> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "-z", "--untracked-files=all"])
        .current_dir(workspace)
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let mut paths = Vec::new();
    for item in out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        if item.len() > 3 {
            let entry = String::from_utf8_lossy(&item[3..]);
            // Rename entries read "to -> from"; the worktree path is the target.
            let path = entry
                .split_once(" -> ")
                .map_or(entry.as_ref(), |(_, target)| target);
            paths.push(workspace.join(path));
        }
    }
    Ok(Some(paths))
}
/// The exact HEAD blob for a path, used as pre-change content for files that
/// were clean before a shell command. This reads Git data; it never rewrites
/// history or resets the worktree.
fn git_show_head(workspace: &Path, path: &Path) -> Option<Vec<u8>> {
    let rel = path.strip_prefix(workspace).ok()?;
    let out = std::process::Command::new("git")
        .arg("show")
        .arg(format!("HEAD:{}", rel.to_string_lossy()))
        .current_dir(workspace)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

#[cfg(test)]
#[cfg(test)]
mod tests;
