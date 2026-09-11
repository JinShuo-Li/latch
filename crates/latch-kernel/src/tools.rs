use crate::config::{OutsidePolicy, PermissionConfig};
use crate::store::EventStore;
use anyhow::{Context, Result, anyhow, bail};
use latch_protocol::{ChangeOwner, EventPayload, FileVersion, Mode, ToolCall, ToolResult};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
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
        let mutation = matches!(tool, "patch" | "write" | "undo" | "checkpoint")
            || tool == "shell"
                && !is_read_only_shell(args.get("command").and_then(Value::as_str).unwrap_or(""));
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
        if tool == "shell" {
            let c = args.get("command").and_then(Value::as_str).unwrap_or("");
            if dangerous_shell(c) {
                return PolicyDecision::Deny(
                    "destructive or privileged shell command denied by policy".into(),
                );
            }
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
}
#[derive(Debug, Default)]
struct ChangeLedger {
    initial: HashMap<PathBuf, String>,
    latch: Vec<ChangeRecord>,
    externally_changed: HashSet<PathBuf>,
    checkpoints: Vec<(Uuid, usize)>,
}

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
}
#[derive(Debug, Clone, Copy, Default)]
pub struct ScopeStats {
    pub mutations: usize,
    pub files: usize,
    pub additions: usize,
    pub deletions: usize,
    pub dependency_files: usize,
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
        })
    }
    pub fn set_mode(&self, mode: Mode) {
        self.policy.set_mode(mode);
    }
    pub async fn preexisting_change_count(&self) -> usize {
        self.ledger.lock().await.initial.len()
    }
    pub async fn latch_change_count(&self) -> usize {
        self.ledger.lock().await.latch.len()
    }
    pub async fn scope_stats(&self) -> ScopeStats {
        let ledger = self.ledger.lock().await;
        let files = ledger
            .latch
            .iter()
            .map(|change| &change.path)
            .collect::<HashSet<_>>();
        ScopeStats {
            mutations: ledger.latch.len(),
            files: files.len(),
            additions: ledger.latch.iter().map(|change| change.additions).sum(),
            deletions: ledger.latch.iter().map(|change| change.deletions).sum(),
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
                "Run a bounded Linux developer command. Prefer read_file, search, git_status, and git_diff for inspection; shell is for checks those tools cannot express. ASK/PLAN allow only conservative read-only commands and deny test/build execution.",
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
            return result(call, reason, true, None);
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
        let mut r = match outcome {
            Ok(v) => result(call, v.0, false, v.1),
            Err(e) => result(call, format!("{e:#}"), true, None),
        };
        if call.name == "shell"
            && let Err(error) = self.store.append(
                self.session_id,
                EventPayload::ValidationResult {
                    command: call
                        .arguments
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    passed: !r.is_error,
                    detail: r.output.lines().next().unwrap_or("").into(),
                },
            )
        {
            r = result(
                call,
                format!("persist validation result: {error}"),
                true,
                None,
            );
        }
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
                .latch
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
        self.commit_change(path, before.into(), updated).await
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
        self.commit_change(path, before, content).await
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
    ) -> Result<(String, Option<String>)> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let before_version = before
            .as_ref()
            .map(|b| version(&self.workspace, &path, b))
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
        self.ledger.lock().await.latch.push(ChangeRecord {
            path,
            before,
            after_hash: after_version.content_hash.clone(),
            additions,
            deletions,
        });
        self.store.append(
            self.session_id,
            EventPayload::FileChanged {
                before: before_version,
                after: after_version.clone(),
                owner: ChangeOwner::Latch,
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
            result = tokio::time::timeout(Duration::from_secs(timeout), child.wait()) => Some(result),
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
        if !status.success() {
            bail!("exit {status}\n{bounded}")
        }
        Ok((format!("exit {status}\n{bounded}"), artifact))
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
        let position = l.latch.len();
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
        let change = { self.ledger.lock().await.latch.pop() }
            .ok_or_else(|| anyhow!("no Latch-owned change to undo"))?;
        let current = tokio::fs::read(&change.path).await.ok();
        if current.as_deref().map(hash).as_deref() != Some(&change.after_hash) {
            bail!(
                "cannot undo: {} changed since Latch edit",
                change.path.display()
            );
        }
        if let Some(before) = change.before {
            tokio::fs::write(&change.path, before).await?;
        } else {
            tokio::fs::remove_file(&change.path).await?;
        }
        Ok((
            format!("undid {}", relative(&self.workspace, &change.path)?),
            None,
        ))
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
    let before = String::from_utf8_lossy(before);
    let after = String::from_utf8_lossy(after);
    let before_lines = before.lines().collect::<HashSet<_>>();
    let after_lines = after.lines().collect::<HashSet<_>>();
    (
        after_lines.difference(&before_lines).count(),
        before_lines.difference(&after_lines).count(),
    )
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
/// redirect, background, or nest is rejected, as is `||`. This deliberately
/// keeps read-only classification stricter than the shell's actual grammar so
/// ASK/PLAN can never be used to mutate the workspace.
fn is_read_only_shell(command: &str) -> bool {
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
    segments.iter().all(|segment| is_read_only_command(segment))
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
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "-z"])
        .current_dir(workspace)
        .output()?;
    let mut map = HashMap::new();
    for item in out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        if item.len() > 3 {
            let p = workspace.join(String::from_utf8_lossy(&item[3..]).as_ref());
            if let Ok(b) = std::fs::read(&p) {
                map.insert(p, hash(&b));
            }
        }
    }
    Ok(map)
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
    async fn undo_refuses_external_change() {
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
        for allowed in [
            "git status",
            "git status && git diff",
            "rg foo src | head -n 20",
            "git branch --show-current",
            "git log --oneline -5 | cat",
            "find . -name app.txt",
        ] {
            assert!(is_read_only_shell(allowed), "should allow {allowed}");
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
        ] {
            assert!(!is_read_only_shell(denied), "should deny {denied}");
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
}
