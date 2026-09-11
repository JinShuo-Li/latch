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
                "Run a Linux developer command with cancellation, timeout, and bounded output.",
                json!({"type":"object","required":["command"],"properties":{"command":{"type":"string"},"timeout_seconds":{"type":"integer"}}}),
            ),
            def(
                "git_status",
                "Show concise Git status and diff summary.",
                json!({"type":"object","properties":{}}),
            ),
        ]
    }
    pub async fn execute(&self, call: &ToolCall, cancel: CancellationToken) -> ToolResult {
        let decision = self.policy.decide(&call.name, &call.arguments);
        self.store
            .append(
                self.session_id,
                EventPayload::PermissionDecision {
                    tool: call.name.clone(),
                    decision: format!("{decision:?}"),
                    reason: String::new(),
                },
            )
            .ok();
        if let PolicyDecision::Deny(reason) | PolicyDecision::Ask(reason) = decision {
            return result(call, reason, true, None);
        }
        self.store
            .append(
                self.session_id,
                EventPayload::ToolStarted {
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                },
            )
            .ok();
        let outcome = match call.name.as_str() {
            "read_file" => self.read_file(call).await,
            "search" => self.search(call).await,
            "patch" => self.patch(call).await,
            "write" => self.write(call).await,
            "shell" => self.shell(call, cancel).await,
            "git_status" => self.git_status(call).await,
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
        self.store.append(self.session_id, payload).ok();
        r
    }
    async fn read_file(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _permit = self.read_slots.acquire().await?;
        let path = self.path_arg(call)?;
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        let version = version(&self.workspace, &path, &bytes)?;
        self.observations.lock().await.insert(path, version.clone());
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
        self.ledger.lock().await.latch.push(ChangeRecord {
            path,
            before,
            after_hash: after_version.content_hash.clone(),
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
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        workspace.join(path)
    };
    let canonical_parent = joined
        .parent()
        .unwrap_or(workspace)
        .canonicalize()
        .or_else(|_| canonical_existing_ancestor(joined.parent().unwrap_or(workspace)))?;
    let candidate =
        canonical_parent.join(joined.file_name().ok_or_else(|| anyhow!("invalid path"))?);
    let root = workspace.canonicalize()?;
    if !candidate.starts_with(&root) {
        bail!("path escapes workspace");
    }
    Ok(candidate)
}
fn canonical_existing_ancestor(mut p: &Path) -> Result<PathBuf> {
    loop {
        if let Ok(v) = p.canonicalize() {
            return Ok(v);
        }
        p = p
            .parent()
            .ok_or_else(|| anyhow!("no existing path ancestor"))?;
    }
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
fn is_read_only_shell(c: &str) -> bool {
    if c.chars()
        .any(|ch| matches!(ch, ';' | '&' | '|' | '$' | '`' | '<' | '>'))
    {
        return false;
    }
    let mut words = c.split_whitespace();
    let first = words.next().unwrap_or("");
    if first != "git" {
        return matches!(
            first,
            "rg" | "grep" | "find" | "ls" | "pwd" | "sed" | "head" | "tail" | "wc"
        );
    }
    matches!(
        words.next().unwrap_or(""),
        "status" | "diff" | "log" | "show" | "rev-parse" | "branch" | "ls-files"
    )
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
