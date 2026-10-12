//! Managed processes and sandboxed command execution. Process status/cursor
//! transitions stay here; platform cleanup and shell syntax live in execution.

use super::*;
use std::io::Write as _;

/// How long a terminated or exited process's output readers may take to see
/// EOF. Bubblewrap's namespace init can be interrupted before it execs the
/// command and then block forever on its internal sync pipe, holding the
/// inherited stdout/stderr write ends; a bounded drain keeps `exec_terminate`
/// and `exec_poll` honest instead of hanging on that orphan.
const PROCESS_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Waits for the output readers with a bound, aborting a reader that can never
/// reach EOF because an orphaned sandbox process still holds the pipe.
pub(super) async fn drain_readers(readers: Vec<tokio::task::JoinHandle<()>>) {
    for mut handle in readers {
        if tokio::time::timeout(PROCESS_DRAIN_TIMEOUT, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
        }
    }
}

impl ToolExecutor {
    /// Starts a persistent development process owned by the kernel. Output is
    /// buffered for `exec_poll`; lifecycle is durable so a resumed session can
    /// report honestly that the child did not survive the restart.
    pub(super) async fn process_start(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let command = str_arg(call, "command")?.to_owned();
        let label = call
            .arguments
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let id = format!("proc-{}", Uuid::new_v4());
        let profile = self.command_profile(self.sandbox_profile(call), &command);
        let runner = self.sandbox_runner()?;
        let may_write_workspace = profile.workspace_writable();
        if may_write_workspace {
            self.store.append(
                self.session_id,
                EventPayload::WorkspaceMutationPossible {
                    operation: format!("exec_start: {command}"),
                },
            )?;
        }
        let mut child = runner
            .command(&profile, &command)?
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start process: {command}"))?;
        let pid = child.id();
        let output = Arc::new(Mutex::new(ProcessBuffer::new()));
        let artifact_name = format!("process-{id}.log");
        let mut readers = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            readers.push(spawn_reader(
                stdout,
                output.clone(),
                self.artifacts.clone(),
                artifact_name.clone(),
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.push(spawn_reader(
                stderr,
                output.clone(),
                self.artifacts.clone(),
                artifact_name.clone(),
            ));
        }
        self.store.append(
            self.session_id,
            EventPayload::ProcessStarted {
                id: id.clone(),
                command: command.clone(),
                label: label.clone(),
                may_write_workspace: Some(may_write_workspace),
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
                exit_recorded: false,
            },
        );
        // A model may never poll this process. Keep only a weak owner reference
        // so the watcher cannot keep an abandoned executor (and its children)
        // alive. Lifecycle commits remain serialized with poll/terminate.
        let processes = Arc::downgrade(&self.processes);
        let store = self.store.clone();
        let session_id = self.session_id;
        let watched_id = id.clone();
        tokio::spawn(async move {
            loop {
                {
                    let Some(processes) = processes.upgrade() else {
                        break;
                    };
                    let mut processes = processes.lock().await;
                    let Some(process) = processes.get_mut(&watched_id) else {
                        break;
                    };
                    // A failed durable commit must remain retryable, never a
                    // successful in-memory exit with no corresponding event.
                    let _ = refresh_process(&store, session_id, &watched_id, process).await;
                    if process.exit_recorded {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
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
    pub(super) async fn process_poll(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
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
        refresh_process(&self.store, self.session_id, id, process).await?;
        let output = process.output.lock().await;
        let start = process.cursor.min(output.text.len());
        let mut end = start
            .saturating_add(PROCESS_POLL_MAX_BYTES)
            .min(output.text.len());
        while end > start && !output.text.is_char_boundary(end) {
            end -= 1;
        }
        let new = output.text[start..end].to_owned();
        process.cursor = end;
        let truncated = end < output.text.len();
        let spill_failed = output.spill_failed;
        drop(output);
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
            if truncated {
                text.push_str("\n[more retained output; call exec_poll again]");
            }
        }
        if let Some(artifact) = &process.artifact_id {
            text.push_str(&format!("\n[full output artifact: {artifact}]"));
        } else if spill_failed {
            text.push_str(&format!(
                "\n[output beyond {PROCESS_MAX_BUFFER_BYTES} bytes was dropped: the overflow artifact could not be written]"
            ));
        }
        Ok((text, process.artifact_id.clone()))
    }
    pub(super) async fn process_terminate(
        &self,
        call: &ToolCall,
    ) -> Result<(String, Option<String>)> {
        let id = str_arg(call, "id")?;
        let mut processes = self.processes.lock().await;
        let Some(process) = processes.get_mut(id) else {
            if self.process_was_started(id)? {
                bail!("process {id} is not running; it did not survive a Latch restart");
            }
            bail!("unknown process {id}");
        };
        refresh_process(&self.store, self.session_id, id, process).await?;
        if process.status == ProcessStatus::Running {
            if let Some(child) = process.child.as_mut() {
                crate::execution::kill_sandbox_children(child.id());
                child.kill().await?;
                child.wait().await?;
            }
            process.status = ProcessStatus::Killed;
            process.child = None;
            drain_readers(std::mem::take(&mut process.readers)).await;
        }
        refresh_process(&self.store, self.session_id, id, process).await?;
        Ok((
            format!("[process {id}: {}]", process.status.label()),
            process.artifact_id.clone(),
        ))
    }
    /// Reconcile natural exits before taking a validation snapshot, including
    /// the interval before the asynchronous watcher gets its next time slice.
    pub(crate) async fn refresh_managed_processes(&self) -> Result<()> {
        let mut processes = self.processes.lock().await;
        for (id, process) in processes.iter_mut() {
            refresh_process(&self.store, self.session_id, id, process).await?;
        }
        Ok(())
    }
    pub(super) fn process_was_started(&self, id: &str) -> Result<bool> {
        Ok(self
                .store
                .events(self.session_id)?
                .iter()
                .any(|event| matches!(&event.payload, EventPayload::ProcessStarted { id: started, .. } if started == id)))
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
        // Shared by root and child executors: validation cannot race a
        // synchronous shell write or a guarded workspace edit.
        let _workspace_guard = self.mutation_lock.lock().await;
        let profile = self.command_profile(profile.clone(), command);
        let drift = profile.workspace_writable();
        let before = if drift {
            self.snapshot_dirty().await.ok().flatten()
        } else {
            None
        };
        if drift {
            self.store.append(
                self.session_id,
                EventPayload::WorkspaceMutationPossible {
                    operation: format!("shell: {command}"),
                },
            )?;
        }
        let started = Instant::now();
        let (status, text, artifact) = self
            .run_process_inner(&profile, command, timeout_seconds, cancel)
            .await?;
        if drift {
            self.classify_drift(command, before.as_deref()).await?;
        }
        Ok(ProcessOutput {
            success: status.success(),
            status_line: format!("exit code {}", status.code().unwrap_or(-1)),
            text,
            artifact_id: artifact,
            elapsed: started.elapsed(),
        })
    }

    fn command_profile(&self, profile: SandboxProfile, command: &str) -> SandboxProfile {
        // A name-based inspection classifier cannot prove that Git's configured
        // helpers are harmless. Its read-only promise must be enforced by the
        // backend, rather than used to skip mutation tracking on a write mount.
        if is_read_only_shell(command, &self.workspace)
            && !profile.capabilities.any(&[
                crate::sandbox::Capability::GitMetadataWrite,
                crate::sandbox::Capability::WorkspaceMetadataWrite,
                crate::sandbox::Capability::ExternalFilesystemWrite,
            ])
        {
            profile.read_only_filesystem()
        } else {
            profile
        }
    }
    pub(super) async fn run_process_inner(
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
            .command(profile, command)?
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
        let profile = self.command_profile(self.sandbox_profile(call), &command);
        let command = crate::execution::validation_command(&command)?;
        self.run_process(&profile, &command, timeout_seconds, cancel)
            .await
    }
    pub(super) async fn shell(
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
    pub(crate) fn bound_output(
        &self,
        text: String,
        prefix: &str,
    ) -> Result<(String, Option<String>)> {
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
}

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
pub(super) struct ManagedProcess {
    label: String,
    status: ProcessStatus,
    child: Option<Child>,
    output: Arc<Mutex<ProcessBuffer>>,
    readers: Vec<tokio::task::JoinHandle<()>>,
    /// Byte offset already returned to the model by `exec_poll`.
    cursor: usize,
    artifact_id: Option<String>,
    /// Set only after the durable exit event commits. Failed commits retry.
    exit_recorded: bool,
}

async fn refresh_process(
    store: &EventStore,
    session_id: Uuid,
    id: &str,
    process: &mut ManagedProcess,
) -> Result<()> {
    if process.exit_recorded {
        return Ok(());
    }
    if process.status == ProcessStatus::Running {
        let Some(child) = process.child.as_mut() else {
            bail!("running managed process {id} has no child");
        };
        let Some(status) = child.try_wait()? else {
            // Still running: adopt the streaming artifact as soon as overflow
            // starts so polls can reference it.
            adopt_artifact(process).await;
            return Ok(());
        };
        process.status = ProcessStatus::Exited(status.code().unwrap_or(-1));
        process.child = None;
        drain_readers(std::mem::take(&mut process.readers)).await;
    }
    adopt_artifact(process).await;
    let status = match process.status {
        ProcessStatus::Running => unreachable!("running processes are not finalized"),
        ProcessStatus::Exited(code) => format!("exit {code}"),
        ProcessStatus::Killed => "killed".into(),
    };
    store.append(
        session_id,
        EventPayload::ProcessExited {
            id: id.to_owned(),
            status,
            artifact_id: process.artifact_id.clone(),
        },
    )?;
    process.exit_recorded = true;
    Ok(())
}

/// Adopts the streaming overflow artifact, if any, as the process artifact.
async fn adopt_artifact(process: &mut ManagedProcess) {
    let output = process.output.lock().await;
    if process.artifact_id.is_none() {
        process.artifact_id.clone_from(&output.artifact);
    }
}

/// Managed process output retained in memory before spilling to an artifact.
const PROCESS_MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Maximum bytes returned by one `exec_poll`. Larger output stays readable
/// through the process artifact, matching the bounded-output convention used
/// by the other tools.
const PROCESS_POLL_MAX_BYTES: usize = 24_000;

/// Combined stdout/stderr of one managed process: a bounded in-memory prefix
/// plus an append-only artifact holding everything beyond it. A chatty process
/// can therefore neither grow session memory without limit nor inject an
/// unbounded result into the durable event log in one poll.
#[derive(Debug)]
pub(super) struct ProcessBuffer {
    text: String,
    /// Artifact name once output exceeded the retained prefix.
    artifact: Option<String>,
    /// Open append handle for the overflow artifact.
    overflow: Option<std::fs::File>,
    /// Set when output exceeded the cap but could not be streamed to disk.
    /// The excess is dropped rather than retained; polls report it honestly.
    spill_failed: bool,
}

impl ProcessBuffer {
    pub(super) fn new() -> Self {
        Self {
            text: String::new(),
            artifact: None,
            overflow: None,
            spill_failed: false,
        }
    }

    /// Appends already-decoded text, spilling to the artifact once the
    /// in-memory prefix reaches `PROCESS_MAX_BUFFER_BYTES`.
    fn push_text(&mut self, chunk: &str, artifacts: &Path, artifact_name: &str) {
        if let Some(file) = self.overflow.as_mut() {
            if file.write_all(chunk.as_bytes()).is_err() {
                self.overflow = None;
                self.spill_failed = true;
            }
            return;
        }
        if self.spill_failed {
            return;
        }
        let keep = PROCESS_MAX_BUFFER_BYTES.saturating_sub(self.text.len());
        if chunk.len() <= keep {
            self.text.push_str(chunk);
            return;
        }
        // First overflow: the artifact must hold the complete stream, so the
        // already-retained prefix is written before the new chunk.
        let start = std::fs::File::create(artifacts.join(artifact_name));
        let Ok(mut file) = start else {
            self.spill_failed = true;
            return;
        };
        if file.write_all(self.text.as_bytes()).is_err()
            || file.write_all(chunk.as_bytes()).is_err()
        {
            self.spill_failed = true;
            return;
        }
        let boundary = floor_char_boundary(chunk, keep);
        self.text.push_str(&chunk[..boundary]);
        self.overflow = Some(file);
        self.artifact = Some(artifact_name.to_owned());
    }
}

/// Largest byte index `<= index` that is a char boundary in `text`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Incremental UTF-8 decoder for one output stream. Read chunks can split a
/// multi-byte character, so an incomplete trailing sequence is carried to the
/// next read instead of being decoded as a replacement character; genuinely
/// invalid bytes still become U+FFFD, exactly like `String::from_utf8_lossy`.
#[derive(Debug, Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(valid) => {
                    text.push_str(valid);
                    self.pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    text.push_str(
                        std::str::from_utf8(&self.pending[..valid]).expect("valid UTF-8 prefix"),
                    );
                    match error.error_len() {
                        Some(invalid) => {
                            text.push('\u{FFFD}');
                            self.pending.drain(..valid + invalid);
                        }
                        None => {
                            // An incomplete character at the end: keep it for
                            // the next read.
                            self.pending.drain(..valid);
                            break;
                        }
                    }
                }
            }
        }
        text
    }

    /// Decodes any trailing incomplete sequence at EOF, matching a
    /// whole-stream lossy decode.
    fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

pub(super) fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    output: Arc<Mutex<ProcessBuffer>>,
    artifacts: PathBuf,
    artifact_name: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut decoder = Utf8Decoder::default();
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = decoder.decode(&buffer[..n]);
                    if !text.is_empty() {
                        output
                            .lock()
                            .await
                            .push_text(&text, &artifacts, &artifact_name);
                    }
                }
            }
        }
        let tail = decoder.flush();
        if !tail.is_empty() {
            output
                .lock()
                .await
                .push_text(&tail, &artifacts, &artifact_name);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_process(dir: &Path, text: &str, artifact: Option<&str>) -> ManagedProcess {
        let mut output = ProcessBuffer::new();
        if !text.is_empty() {
            output.push_text(text, dir, "process-fixture.log");
        }
        ManagedProcess {
            label: String::new(),
            status: ProcessStatus::Exited(0),
            child: None,
            output: Arc::new(Mutex::new(output)),
            readers: Vec::new(),
            cursor: 0,
            artifact_id: artifact.map(str::to_owned),
            exit_recorded: false,
        }
    }

    #[tokio::test]
    async fn failed_exit_commit_is_retryable_and_never_reported_twice() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(dir.path()).unwrap();
        let mut process = fixture_process(dir.path(), "retained", None);
        process.status = ProcessStatus::Exited(7);
        store.fail_appends(true);
        assert!(
            refresh_process(&store, session, "fixture", &mut process)
                .await
                .is_err()
        );
        assert!(!process.exit_recorded);
        assert!(store.events(session).unwrap().is_empty());
        store.fail_appends(false);
        refresh_process(&store, session, "fixture", &mut process)
            .await
            .unwrap();
        refresh_process(&store, session, "fixture", &mut process)
            .await
            .unwrap();
        assert!(process.exit_recorded);
        assert_eq!(store.events(session).unwrap().len(), 1);
        assert!(matches!(&store.events(session).unwrap()[0].payload,
            EventPayload::ProcessExited { status, .. } if status == "exit 7"
        ));
        assert_eq!(process.output.lock().await.text, "retained");
    }

    #[test]
    fn managed_process_output_stays_bounded_and_spills_to_the_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let mut output = ProcessBuffer::new();
        let chunk = "x".repeat(4096);
        let total = PROCESS_MAX_BUFFER_BYTES + 3 * 4096;
        for _ in 0..(total / chunk.len()) {
            output.push_text(&chunk, dir.path(), "process-fixture.log");
        }
        assert!(
            output.text.len() <= PROCESS_MAX_BUFFER_BYTES,
            "in-memory output must stay capped: {}",
            output.text.len()
        );
        assert_eq!(output.artifact.as_deref(), Some("process-fixture.log"));
        let stored = std::fs::read(dir.path().join("process-fixture.log")).unwrap();
        assert_eq!(
            stored.len(),
            total,
            "the artifact must hold the full stream"
        );
        assert!(stored.iter().all(|byte| *byte == b'x'));
    }

    #[tokio::test]
    async fn exec_poll_bounds_each_response_and_references_the_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(dir.path()).unwrap();
        let executor = ToolExecutor::new_with_state_dir(
            dir.path().into(),
            artifacts.clone(),
            dir.path().join("state"),
            store.clone(),
            session,
            PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        // Enough output to span three bounded polls while remaining under the
        // in-memory cap, so this exercises poll bounding rather than spilling.
        let total = 2 * PROCESS_POLL_MAX_BYTES + 100;
        let process = fixture_process(dir.path(), &"z".repeat(total), None);
        executor
            .processes
            .lock()
            .await
            .insert("fixture".into(), process);
        let mut seen = String::new();
        let mut polls = 0;
        loop {
            let poll = executor
                .execute(
                    &ToolCall {
                        id: "poll".into(),
                        name: "exec_poll".into(),
                        arguments: json!({"id": "fixture"}),
                    },
                    CancellationToken::new(),
                )
                .await;
            assert!(!poll.is_error, "{}", poll.output);
            assert!(
                poll.output.len() <= PROCESS_POLL_MAX_BYTES + 4096,
                "one poll returned {} bytes",
                poll.output.len()
            );
            seen.push_str(&poll.output);
            polls += 1;
            if !poll.output.contains("[more retained output") {
                break;
            }
            assert!(polls < 10, "bounded polls must terminate");
        }
        assert_eq!(polls, 3, "bounded polls cover the whole retained output");
        assert_eq!(
            seen.matches('z').count(),
            total,
            "polling must return every retained byte exactly once"
        );
        // The exited process's exit event is durable and carries the artifact.
        assert!(store.events(session).unwrap().iter().any(|event| matches!(
            &event.payload,
            EventPayload::ProcessExited { id, .. } if id == "fixture"
        )));
    }

    #[tokio::test]
    async fn exec_poll_references_the_streaming_artifact_after_exit() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(dir.path()).unwrap();
        let executor = ToolExecutor::new_with_state_dir(
            dir.path().into(),
            artifacts.clone(),
            dir.path().join("state"),
            store.clone(),
            session,
            PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
        )
        .unwrap();
        let mut output = ProcessBuffer::new();
        output.push_text(
            &"q".repeat(PROCESS_MAX_BUFFER_BYTES + 4096),
            &artifacts,
            "process-fixture.log",
        );
        let mut process = fixture_process(dir.path(), "", None);
        process.output = Arc::new(Mutex::new(output));
        executor
            .processes
            .lock()
            .await
            .insert("fixture".into(), process);
        let poll = executor
            .execute(
                &ToolCall {
                    id: "poll".into(),
                    name: "exec_poll".into(),
                    arguments: json!({"id": "fixture"}),
                },
                CancellationToken::new(),
            )
            .await;
        assert!(!poll.is_error, "{}", poll.output);
        assert!(
            poll.output
                .contains("[full output artifact: process-fixture.log]"),
            "{}",
            poll.output
        );
        assert_eq!(poll.artifact_id.as_deref(), Some("process-fixture.log"));
        assert!(
            store.events(session).unwrap().iter().any(|event| matches!(
                &event.payload,
                EventPayload::ProcessExited { id, artifact_id: Some(name), .. }
                    if id == "fixture" && name == "process-fixture.log"
            )),
            "the durable exit event must carry the artifact"
        );
    }

    /// CR-07 regression: a multi-byte character split across two reads must be
    /// reassembled, not decoded as two replacement characters.
    #[test]
    fn utf8_split_across_reads_is_reassembled() {
        let mut decoder = Utf8Decoder::default();
        let bytes = "日本語".as_bytes();
        assert_eq!(decoder.decode(&bytes[..4]), "日");
        assert_eq!(decoder.decode(&bytes[4..]), "本語");
        assert_eq!(decoder.flush(), "");
    }

    #[test]
    fn utf8_invalid_bytes_become_replacement_characters() {
        let mut decoder = Utf8Decoder::default();
        assert_eq!(decoder.decode(&[0xFF, 0xFE, b'a']), "\u{FFFD}\u{FFFD}a");
        assert_eq!(decoder.flush(), "");
    }

    #[test]
    fn utf8_incomplete_tail_flushes_as_replacement_at_eof() {
        let mut decoder = Utf8Decoder::default();
        assert_eq!(decoder.decode(&[b'a', 0xE6, 0x97]), "a");
        assert_eq!(decoder.flush(), "\u{FFFD}");
    }

    #[tokio::test]
    async fn spawn_reader_reassembles_multibyte_output_split_across_reads() {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let output = Arc::new(Mutex::new(ProcessBuffer::new()));
        let payload = format!("{}éb", "a".repeat(4095));
        let bytes = payload.as_bytes();
        // The first 4096 bytes end between the two bytes of `é`.
        assert!(bytes.len() > 4096);
        let (mut writer, reader) = tokio::io::duplex(8192);
        let handle = spawn_reader(
            reader,
            output.clone(),
            dir.path().into(),
            "process-fixture.log".into(),
        );
        writer.write_all(&bytes[..4096]).await.unwrap();
        writer.write_all(&bytes[4096..]).await.unwrap();
        drop(writer);
        handle.await.unwrap();
        let retained = output.lock().await.text.clone();
        assert_eq!(retained, payload);
        assert!(!retained.contains('\u{FFFD}'));
    }
}
