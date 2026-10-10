//! Managed processes and sandboxed command execution. Process status/cursor
//! transitions stay here; platform cleanup and shell syntax live in execution.

use super::*;

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
        let artifacts = self.artifacts.clone();
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
                    let _ =
                        refresh_process(&store, session_id, &artifacts, &watched_id, process).await;
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
        refresh_process(&self.store, self.session_id, &self.artifacts, id, process).await?;
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
        refresh_process(&self.store, self.session_id, &self.artifacts, id, process).await?;
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
        refresh_process(&self.store, self.session_id, &self.artifacts, id, process).await?;
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
            refresh_process(&self.store, self.session_id, &self.artifacts, id, process).await?;
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
    output: Arc<Mutex<String>>,
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
    artifacts: &Path,
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
            return Ok(());
        };
        process.status = ProcessStatus::Exited(status.code().unwrap_or(-1));
        process.child = None;
        drain_readers(std::mem::take(&mut process.readers)).await;
    }
    let full = process.output.lock().await.clone();
    if full.len() > PROCESS_MAX_BUFFER_BYTES {
        let name = format!("process-{id}.log");
        std::fs::write(artifacts.join(&name), full.as_bytes())?;
        process.artifact_id = Some(name);
    }
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

/// Managed process output retained in memory before spilling to an artifact.
const PROCESS_MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_exit_commit_is_retryable_and_never_reported_twice() {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open_memory().unwrap();
        let session = store.create_session(dir.path()).unwrap();
        let mut process = ManagedProcess {
            label: String::new(),
            status: ProcessStatus::Exited(7),
            child: None,
            output: Arc::new(Mutex::new("retained".into())),
            readers: Vec::new(),
            cursor: 0,
            artifact_id: None,
            exit_recorded: false,
        };
        store.fail_appends(true);
        assert!(
            refresh_process(&store, session, dir.path(), "fixture", &mut process)
                .await
                .is_err()
        );
        assert!(!process.exit_recorded);
        assert!(store.events(session).unwrap().is_empty());
        store.fail_appends(false);
        refresh_process(&store, session, dir.path(), "fixture", &mut process)
            .await
            .unwrap();
        refresh_process(&store, session, dir.path(), "fixture", &mut process)
            .await
            .unwrap();
        assert!(process.exit_recorded);
        assert_eq!(store.events(session).unwrap().len(), 1);
        assert!(matches!(&store.events(session).unwrap()[0].payload,
            EventPayload::ProcessExited { status, .. } if status == "exit 7"
        ));
        assert_eq!(process.output.lock().await.as_str(), "retained");
    }
}
