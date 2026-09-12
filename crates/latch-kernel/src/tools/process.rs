//! Managed processes, sandboxed command execution, and the read-only shell
//! classifier. Process status/cursor transitions stay in this one module.

use super::*;

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
    pub(super) async fn finish_process(
        &self,
        id: &str,
        process: &mut ManagedProcess,
    ) -> Result<()> {
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
    pub(super) fn bound_output(
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

pub(super) fn split_simple_commands(command: &str) -> Option<Vec<&str>> {
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

pub(super) fn is_read_only_command(command: &str) -> bool {
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
