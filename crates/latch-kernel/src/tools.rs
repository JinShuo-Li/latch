use crate::config::{OutsidePolicy, PermissionConfig};
use crate::store::EventStore;
use anyhow::{Context, Result, anyhow, bail};
use latch_protocol::{ChangeOwner, EventPayload, FileVersion, Mode, ToolCall, ToolResult};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny(String),
    Ask(String),
}
#[derive(Debug, Clone)]
pub struct PolicyEngine {
    mode: Arc<RwLock<Mode>>,
    workspace: PathBuf,
    config: PermissionConfig,
}
impl PolicyEngine {
    #[must_use]
    pub fn new(mode: Mode, workspace: PathBuf, config: PermissionConfig) -> Self {
        Self {
            mode: Arc::new(RwLock::new(mode)),
            workspace,
            config,
        }
    }
    pub fn set_mode(&self, mode: Mode) {
        if let Ok(mut current) = self.mode.write() {
            *current = mode;
        }
    }
    #[must_use]
    pub fn decide(&self, tool: &str, args: &Value) -> PolicyDecision {
        let command = || args.get("command").and_then(Value::as_str).unwrap_or("");
        let mutation = matches!(tool, "patch" | "write" | "undo" | "checkpoint")
            || matches!(tool, "shell" | "validate")
                && !is_read_only_shell(command(), &self.workspace);
        let mode = self.mode.read().map_or(Mode::Ask, |m| *m);
        if mutation && !mode.can_mutate() {
            return PolicyDecision::Deny(format!("{mode} mode cannot mutate the workspace"));
        }
        if matches!(tool, "patch" | "write")
            && let Some(path) = args.get("path").and_then(Value::as_str)
            && resolve_workspace_path(&self.workspace, path).is_err()
        {
            return match self.config.outside_workspace {
                OutsidePolicy::Deny => PolicyDecision::Deny("path escapes workspace".into()),
                OutsidePolicy::Ask => {
                    PolicyDecision::Ask("outside-workspace write requires explicit approval".into())
                }
            };
        }
        if matches!(tool, "shell" | "validate") && dangerous_shell(command()) {
            return PolicyDecision::Deny(
                "destructive or privileged shell command denied by policy".into(),
            );
        }
        if mutation && !self.config.workspace_write {
            return PolicyDecision::Deny("workspace writes disabled by configuration".into());
        }
        PolicyDecision::Allow
    }
}

#[derive(Debug, Clone)]
struct ChangeRecord {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after_hash: String,
    additions: usize,
    deletions: usize,
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

#[derive(Clone)]
pub struct ToolExecutor {
    workspace: PathBuf,
    artifacts: PathBuf,
    store: EventStore,
    session_id: Uuid,
    policy: PolicyEngine,
    observations: Arc<Mutex<HashMap<PathBuf, FileVersion>>>,
    ledger: Arc<Mutex<ChangeLedger>>,
    mutation_lock: Arc<Mutex<()>>,
    read_slots: Arc<Semaphore>,
    restored: Arc<std::sync::atomic::AtomicBool>,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct ScopeStats {
    pub mutations: usize,
    pub files: usize,
    pub additions: usize,
    pub deletions: usize,
    pub dependency_files: usize,
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
        Ok(Self {
            workspace,
            artifacts,
            store,
            session_id,
            policy,
            observations: Arc::new(Mutex::new(HashMap::new())),
            ledger: Arc::new(Mutex::new(ChangeLedger {
                initial,
                ..Default::default()
            })),
            mutation_lock: Arc::new(Mutex::new(())),
            read_slots: Arc::new(Semaphore::new(8)),
            restored: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
    pub fn set_mode(&self, mode: Mode) {
        self.policy.set_mode(mode);
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
                    additions,
                    deletions,
                    ..
                } if !matches!(owner, ChangeOwner::External) => {
                    let path = self.workspace.join(&after.path);
                    let record = ChangeRecord {
                        path,
                        before: None,
                        after_hash: after.content_hash.clone(),
                        additions: *additions,
                        deletions: *deletions,
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
    pub async fn scope_stats(&self) -> ScopeStats {
        let ledger = self.ledger.lock().await;
        let files = ledger
            .owned
            .iter()
            .map(|change| &change.path)
            .collect::<HashSet<_>>();
        ScopeStats {
            mutations: ledger.owned.len(),
            files: files.len(),
            additions: ledger.owned.iter().map(|change| change.additions).sum(),
            deletions: ledger.owned.iter().map(|change| change.deletions).sum(),
            dependency_files: files.iter().filter(|path| is_dependency_file(path)).count(),
        }
    }
    #[must_use]
    pub fn definitions() -> Vec<latch_protocol::ToolDefinition> {
        vec![
            def(
                "read_file",
                "Read a UTF-8 workspace file and return its content plus a version hash.",
                json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
            ),
            def(
                "search",
                "Search repository text with ripgrep.",
                json!({"type":"object","required":["query"],"properties":{"query":{"type":"string"},"path":{"type":"string"}}}),
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
        let decision = self.policy.decide(&call.name, &call.arguments);
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
            "patch" => self.patch(call).await,
            "write" => self.write(call).await,
            "shell" => self.shell(call, cancel).await,
            "git_status" => self.git_status(call).await,
            "git_diff" => self.git_diff(call).await,
            "checkpoint" => self.checkpoint(call).await,
            "undo" => self.undo(call).await,
            _ => Err(anyhow!("unknown tool {}", call.name)),
        };
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
        Ok((format!("hash: {}\n{}", version.content_hash, text), None))
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
        let out = Command::new("rg")
            .args(["-n", "--color=never", "--", q])
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
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        self.bound_output(text, "search")
    }
    async fn patch(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _guard = self.mutation_lock.lock().await;
        let path = self.path_arg(call)?;
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
        self.commit_change(path, Some(before), updated, ChangeOwner::Latch)
            .await
    }
    async fn write(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _guard = self.mutation_lock.lock().await;
        let path = self.path_arg(call)?;
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
        self.commit_change(path, before, content, ChangeOwner::Latch)
            .await
    }
    async fn ensure_fresh(&self, path: &Path, bytes: &[u8], base: &str) -> Result<()> {
        let actual = hash(bytes);
        if actual != base {
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
        let (additions, deletions) = line_delta(before.as_deref().unwrap_or_default(), &after);
        self.ledger.lock().await.owned.push(ChangeRecord {
            path: path.clone(),
            before,
            after_hash: after_version.content_hash.clone(),
            additions,
            deletions,
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
            .run_process_inner(command, timeout_seconds, cancel)
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
        command: &str,
        timeout_seconds: u64,
        cancel: CancellationToken,
    ) -> Result<(std::process::ExitStatus, String, Option<String>)> {
        let op = self
            .store
            .begin_operation(self.session_id, &format!("shell: {command}"))?;
        let mut child = Command::new("bash")
            .args(["-lc", command])
            .current_dir(&self.workspace)
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
        let output = self.run_process(command, timeout, cancel).await?;
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
            self.ledger.lock().await.owned.push(ChangeRecord {
                path: path.clone(),
                before: before_bytes,
                after_hash: after.content_hash.clone(),
                additions,
                deletions,
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
    async fn git_status(&self, _: &ToolCall) -> Result<(String, Option<String>)> {
        let out = Command::new("git")
            .args(["status", "--short", "--branch"])
            .current_dir(&self.workspace)
            .output()
            .await?;
        let diff = Command::new("git")
            .args(["diff", "--stat"])
            .current_dir(&self.workspace)
            .output()
            .await?;
        Ok((
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&diff.stdout)
            ),
            None,
        ))
    }
    async fn git_diff(&self, _: &ToolCall) -> Result<(String, Option<String>)> {
        let output = Command::new("git")
            .args(["diff", "--no-ext-diff", "--"])
            .current_dir(&self.workspace)
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
    Ok(path
        .strip_prefix(root.canonicalize()?)?
        .to_string_lossy()
        .into_owned())
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
fn is_dependency_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("Cargo.toml" | "package.json" | "pyproject.toml" | "go.mod")
    )
}
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
fn dangerous_shell(c: &str) -> bool {
    let l = c.to_ascii_lowercase();
    let words = l.split_whitespace().collect::<Vec<_>>();
    l.starts_with("sudo ")
        || (words.first() == Some(&"rm") && words.iter().any(|word| word.contains('r')))
        || (words.starts_with(&["git", "push"])
            && words.iter().any(|word| word.starts_with("--force")))
        || l.contains("git reset --hard")
        || l.contains("git clean -f")
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
mod tests {
    use super::*;
    use crate::config::PermissionConfig;
    use tempfile::tempdir;
    fn setup(mode: Mode) -> (tempfile::TempDir, ToolExecutor) {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "old").unwrap();
        let s = EventStore::open_memory().unwrap();
        let id = s.create_session(d.path()).unwrap();
        let p = PolicyEngine::new(mode, d.path().into(), PermissionConfig::default());
        let e = ToolExecutor::new(d.path().into(), d.path().join("artifacts"), s, id, p).unwrap();
        (d, e)
    }
    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: args,
        }
    }
    #[tokio::test]
    async fn ask_and_plan_deny_mutation() {
        for mode in [Mode::Ask, Mode::Plan] {
            let (_d, e) = setup(mode);
            let r = e
                .execute(
                    &call("write", json!({"path":"x","content":"x"})),
                    CancellationToken::new(),
                )
                .await;
            assert!(r.is_error);
        }
    }
    #[tokio::test]
    async fn work_allows_guarded_edit_and_rejects_stale() {
        let (d, e) = setup(Mode::Work);
        let read = e
            .execute(
                &call("read_file", json!({"path":"a.txt"})),
                CancellationToken::new(),
            )
            .await;
        let h = read
            .output
            .lines()
            .next()
            .unwrap()
            .strip_prefix("hash: ")
            .unwrap();
        let ok = e
            .execute(
                &call(
                    "patch",
                    json!({"path":"a.txt","base_hash":h,"old":"old","new":"new"}),
                ),
                CancellationToken::new(),
            )
            .await;
        assert!(!ok.is_error);
        std::fs::write(d.path().join("a.txt"), "external").unwrap();
        let stale = e
            .execute(
                &call(
                    "patch",
                    json!({"path":"a.txt","base_hash":h,"old":"old","new":"bad"}),
                ),
                CancellationToken::new(),
            )
            .await;
        assert!(stale.output.contains("stale observation"));
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "external"
        );
    }
    #[tokio::test]
    async fn undo_only_own_unchanged_result() {
        let (d, e) = setup(Mode::Work);
        let read = e
            .execute(
                &call("read_file", json!({"path":"a.txt"})),
                CancellationToken::new(),
            )
            .await;
        let h = read
            .output
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("hash: ");
        e.execute(
            &call(
                "patch",
                json!({"path":"a.txt","base_hash":h,"old":"old","new":"new"}),
            ),
            CancellationToken::new(),
        )
        .await;
        let u = e
            .execute(&call("undo", json!({})), CancellationToken::new())
            .await;
        assert!(!u.is_error);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "old"
        );
    }
    #[tokio::test]
    async fn undo_refuses_external_change_and_keeps_record() {
        let (d, e) = setup(Mode::Work);
        let read = e
            .execute(
                &call("read_file", json!({"path":"a.txt"})),
                CancellationToken::new(),
            )
            .await;
        let hash = read
            .output
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("hash: ");
        e.execute(
            &call(
                "patch",
                json!({"path":"a.txt","base_hash":hash,"old":"old","new":"latch"}),
            ),
            CancellationToken::new(),
        )
        .await;
        std::fs::write(d.path().join("a.txt"), "external").unwrap();
        let undo = e
            .execute(&call("undo", json!({})), CancellationToken::new())
            .await;
        assert!(undo.is_error);
        assert_eq!(
            e.latch_change_count().await,
            1,
            "refused undo keeps the record"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "external"
        );
    }

    #[tokio::test]
    async fn read_only_modes_reject_shell_escape() {
        for mode in [Mode::Ask, Mode::Plan] {
            let (_d, executor) = setup(mode);
            for command in [
                "touch injected",
                "git branch -D main",
                "rg x .; touch injected",
                "cargo test",
            ] {
                let result = executor
                    .execute(
                        &call("shell", json!({"command":command})),
                        CancellationToken::new(),
                    )
                    .await;
                assert!(result.is_error, "{mode} allowed {command}");
            }
        }
    }

    #[test]
    fn read_only_shell_classification_is_conservative() {
        let d = tempdir().unwrap();
        std::fs::create_dir(d.path().join("src")).unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("link")).unwrap();
        let ws = d.path().to_path_buf();
        let workspace_cd = format!("cd {} && git log --oneline -20", d.path().display());
        for allowed in [
            "git status",
            "git status && git diff",
            "rg foo src | head -n 20",
            "git branch --show-current",
            "git log --oneline -5 | cat",
            "find . -name app.txt",
            // Workspace-local cd composition is read-only.
            &workspace_cd,
            "cd . && git status --short",
            "cd src && rg normalize_username .",
            "pwd && git diff",
            "git log --oneline -20 | head",
            "cd src && git log --oneline -20",
            "cd src && cd . && pwd",
            "cd src && cd .. && pwd",
            // A cd that returns into the workspace is still workspace-local.
            "cd src; cd ..",
        ] {
            assert!(is_read_only_shell(allowed, &ws), "should allow {allowed}");
        }
        for denied in [
            "git branch -D main",
            "cargo test",
            "rg foo | tee out",
            "git diff > out.patch",
            "git diff --output=out.patch",
            "rg foo || true",
            "rg --pre sh foo",
            "find . -exec rm {} +",
            "find . -fls out.txt",
            "sed -i 's/a/b/' f",
            "sed 1e app.txt",
            "echo $HOME",
            "pwd && touch injected",
            // cd that escapes or cannot be proven safe is denied.
            "cd .. && git log",
            "cd ../outside && git status",
            "cd /tmp && git log",
            "cd / && git status",
            "cd ~ && git status",
            "cd $HOME && git status",
            "cd link && git status",
            "cd /tmp/latch-playground && git log",
            // Non-read-only commands after a safe cd stay denied.
            "cd src && cargo test",
            "cd src && cargo fmt",
            "cd src && sed -i 's/a/b/' f",
            "cd src && git checkout main",
            "cd src && git reset --hard",
            "cd src && git branch -D main",
            "cd src && touch injected",
            "cd src && git diff > out.patch",
            "cd src || git status",
            "cd src && echo hi",
            // Malformed or unprovable cd forms.
            "cd",
            "cd src extra",
            "cd src; cd ../..",
        ] {
            assert!(!is_read_only_shell(denied, &ws), "should deny {denied}");
        }
    }

    #[tokio::test]
    async fn ask_allows_workspace_local_read_only_cd_compound() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "old").unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@l",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(d.path())
            .status()
            .unwrap();
        std::fs::create_dir(d.path().join("src")).unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(d.path()).unwrap();
        let executor = ToolExecutor::new(
            d.path().into(),
            d.path().join("artifacts"),
            store.clone(),
            session,
            PolicyEngine::new(Mode::Ask, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        for command in [
            "cd . && git status --short",
            "cd src && ls",
            "pwd && git diff",
            "git log --oneline -20 | head",
        ] {
            let result = executor
                .execute(
                    &call("shell", json!({"command":command})),
                    CancellationToken::new(),
                )
                .await;
            assert!(
                !result.is_error,
                "ASK should allow {command}: {}",
                result.output
            );
        }
        for command in [
            "cd .. && git status",
            "cd /tmp && git status",
            "cd src && cargo test",
            "cd src && git checkout main",
            "cd src && touch injected",
        ] {
            let result = executor
                .execute(
                    &call("shell", json!({"command":command})),
                    CancellationToken::new(),
                )
                .await;
            assert!(result.is_error, "ASK must deny {command}");
        }
    }

    #[tokio::test]
    async fn ask_allows_conservative_read_only_compound_shell() {
        let (_d, executor) = setup(Mode::Ask);
        let allowed = executor
            .execute(
                &call("shell", json!({"command":"pwd && ls"})),
                CancellationToken::new(),
            )
            .await;
        assert!(!allowed.is_error, "{}", allowed.output);
        let denied = executor
            .execute(
                &call("shell", json!({"command":"pwd && touch injected"})),
                CancellationToken::new(),
            )
            .await;
        assert!(denied.is_error, "{}", denied.output);
    }
    #[tokio::test]
    async fn writes_nested_new_file_without_path_collapse() {
        let (d, executor) = setup(Mode::Work);
        let result = executor
            .execute(
                &call(
                    "write",
                    json!({"path":"new/deep/file.txt","content":"nested"}),
                ),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(d.path().join("new/deep/file.txt")).unwrap(),
            "nested"
        );
    }

    #[tokio::test]
    async fn symlink_cannot_escape_workspace() {
        let (d, executor) = setup(Mode::Work);
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("link")).unwrap();
        let result = executor
            .execute(
                &call("write", json!({"path":"link/escaped.txt","content":"bad"})),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(!outside.path().join("escaped.txt").exists());
    }
    #[tokio::test]
    async fn reread_detects_external_modification() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "first").unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(d.path()).unwrap();
        let executor = ToolExecutor::new(
            d.path().into(),
            d.path().join("art"),
            store.clone(),
            session,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        executor
            .execute(
                &call("read_file", json!({"path":"a.txt"})),
                CancellationToken::new(),
            )
            .await;
        std::fs::write(d.path().join("a.txt"), "second").unwrap();
        executor
            .execute(
                &call("read_file", json!({"path":"a.txt"})),
                CancellationToken::new(),
            )
            .await;
        assert!(store.events(session).unwrap().iter().any(|event| matches!(
            event.payload,
            EventPayload::ExternalFileChangeDetected { .. }
        )));
    }
    #[test]
    fn work_policy_and_dangerous_commands() {
        let (d, _) = setup(Mode::Work);
        let p = PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default());
        assert_eq!(
            p.decide("write", &json!({"path":"a"})),
            PolicyDecision::Allow
        );
        assert!(matches!(
            p.decide("shell", &json!({"command":"sudo rm -rf /"})),
            PolicyDecision::Deny(_)
        ));
    }
    #[tokio::test]
    async fn dirty_workspace_is_recorded() {
        let d = tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .status()
            .unwrap();
        std::fs::write(d.path().join("owned.txt"), "user").unwrap();
        let s = EventStore::open_memory().unwrap();
        let id = s.create_session(d.path()).unwrap();
        let e = ToolExecutor::new(
            d.path().into(),
            d.path().join("art"),
            s,
            id,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        assert_eq!(e.preexisting_change_count().await, 1);
    }

    /// Rebuilds an executor over the same durable store, the way `--resume`
    /// does, and restores ownership from events.
    async fn resumed_executor(
        d: &tempfile::TempDir,
        store: &EventStore,
        session: Uuid,
    ) -> ToolExecutor {
        let executor = ToolExecutor::new(
            d.path().into(),
            d.path().join("artifacts"),
            store.clone(),
            session,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        executor.restore_ownership().await.unwrap();
        executor
    }

    #[tokio::test]
    async fn guarded_edit_undo_works_after_resume() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "old").unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(d.path()).unwrap();
        let e = ToolExecutor::new(
            d.path().into(),
            d.path().join("artifacts"),
            store.clone(),
            session,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        let read = e
            .execute(
                &call("read_file", json!({"path":"a.txt"})),
                CancellationToken::new(),
            )
            .await;
        let h = read
            .output
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("hash: ");
        e.execute(
            &call(
                "patch",
                json!({"path":"a.txt","base_hash":h,"old":"old","new":"latch-owned"}),
            ),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(e.latch_change_count().await, 1);
        drop(e);

        let resumed = resumed_executor(&d, &store, session).await;
        assert_eq!(resumed.latch_change_count().await, 1);
        let undo = resumed
            .execute(&call("undo", json!({})), CancellationToken::new())
            .await;
        assert!(!undo.is_error, "{}", undo.output);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "old"
        );
        assert_eq!(resumed.latch_change_count().await, 0);
    }

    #[tokio::test]
    async fn external_edit_blocks_undo_after_resume() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "old").unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(d.path()).unwrap();
        let e = ToolExecutor::new(
            d.path().into(),
            d.path().join("artifacts"),
            store.clone(),
            session,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        let read = e
            .execute(
                &call("read_file", json!({"path":"a.txt"})),
                CancellationToken::new(),
            )
            .await;
        let h = read
            .output
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("hash: ");
        e.execute(
            &call(
                "patch",
                json!({"path":"a.txt","base_hash":h,"old":"old","new":"latch-owned"}),
            ),
            CancellationToken::new(),
        )
        .await;
        drop(e);
        std::fs::write(d.path().join("a.txt"), "externally edited").unwrap();

        let resumed = resumed_executor(&d, &store, session).await;
        let undo = resumed
            .execute(&call("undo", json!({})), CancellationToken::new())
            .await;
        assert!(undo.is_error);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "externally edited"
        );
    }

    #[tokio::test]
    async fn shell_mutation_is_classified_as_shell_owned_and_undoable() {
        let d = tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@l",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(d.path())
            .status()
            .unwrap();
        std::fs::write(d.path().join("tracked.txt"), "head version").unwrap();
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(d.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@l",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "file",
            ])
            .current_dir(d.path())
            .status()
            .unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(d.path()).unwrap();
        let e = ToolExecutor::new(
            d.path().into(),
            d.path().join("artifacts"),
            store.clone(),
            session,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        let out = e
            .execute(
                &call(
                    "shell",
                    json!({"command":"printf reformatted > tracked.txt"}),
                ),
                CancellationToken::new(),
            )
            .await;
        assert!(!out.is_error, "{}", out.output);
        // The shell-originated mutation is owned by Shell and recorded durably.
        let events = store.events(session).unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::FileChanged { owner, .. } if *owner == ChangeOwner::Shell
        )));
        assert_eq!(e.latch_change_count().await, 1);
        let undo = e
            .execute(&call("undo", json!({})), CancellationToken::new())
            .await;
        assert!(!undo.is_error, "{}", undo.output);
        assert_eq!(
            std::fs::read_to_string(d.path().join("tracked.txt")).unwrap(),
            "head version"
        );
    }

    #[tokio::test]
    async fn non_git_shell_mutation_is_marked_non_reversible() {
        let d = tempdir().unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(d.path()).unwrap();
        let e = ToolExecutor::new(
            d.path().into(),
            d.path().join("artifacts"),
            store.clone(),
            session,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        let out = e
            .execute(
                &call("shell", json!({"command":"printf mutated > plain.txt"})),
                CancellationToken::new(),
            )
            .await;
        assert!(!out.is_error);
        assert!(store.events(session).unwrap().iter().any(|event| matches!(
            &event.payload,
            EventPayload::ShellMutationObserved {
                reversible: false,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn preexisting_dirty_work_is_preserved_and_distinguishable() {
        let d = tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .status()
            .unwrap();
        std::fs::write(d.path().join("pre.txt"), "user work").unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(d.path()).unwrap();
        let e = ToolExecutor::new(
            d.path().into(),
            d.path().join("artifacts"),
            store.clone(),
            session,
            PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        assert_eq!(e.preexisting_change_count().await, 1);
        let out = e
            .execute(
                &call("shell", json!({"command":"printf x >> pre.txt"})),
                CancellationToken::new(),
            )
            .await;
        assert!(!out.is_error, "{}", out.output);
        // The pre-existing file was already captured as dirty, so the shell
        // mutation on it is undoable against the captured bytes.
        assert_eq!(e.latch_change_count().await, 1);
        let undo = e
            .execute(&call("undo", json!({})), CancellationToken::new())
            .await;
        assert!(!undo.is_error, "{}", undo.output);
        assert_eq!(
            std::fs::read_to_string(d.path().join("pre.txt")).unwrap(),
            "user work",
            "undo restores the user's pre-existing content, not HEAD"
        );
    }
}
