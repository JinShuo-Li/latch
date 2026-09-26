use crate::execution::ExecutionBackend;
use crate::sandbox::SandboxProfile;
use anyhow::{Context, Result, anyhow, bail};
use latch_protocol::{EXTENSION_PROTOCOL_VERSION, RpcMessage};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_util::sync::CancellationToken;

const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// Default deadline for process spawn plus the `initialize` request write.
pub const DEFAULT_SPAWN_SECONDS: u64 = 5;
/// Default deadline for the `initialize` response.
pub const DEFAULT_INITIALIZE_SECONDS: u64 = 10;
/// Default deadline for collecting registrations until the `ready` notification.
pub const DEFAULT_READY_SECONDS: u64 = 30;
/// Default deadline for one ordinary extension RPC round trip.
pub const DEFAULT_REQUEST_SECONDS: u64 = 120;
/// Default deadline for the `shutdown` response.
pub const DEFAULT_SHUTDOWN_SECONDS: u64 = 5;
/// Default deadline for graceful child exit after `exit`, before SIGKILL.
pub const DEFAULT_EXIT_SECONDS: u64 = 5;

/// The one central lifecycle policy for extension hosts. Every stage from
/// spawn through exit is bounded, so a silent or broken extension can never
/// pin Latch's startup, RPC handling, cancellation, or shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionLifecycle {
    /// Process spawn plus the `initialize` request write. Process creation is
    /// local and synchronous; the deadline bounds the first handshake write.
    pub spawn: Duration,
    /// The `initialize` response.
    pub initialize: Duration,
    /// Registration stream from `initialized` until the `ready` notification.
    pub ready: Duration,
    /// One ordinary request (`tool.execute`, guard, transform, context source).
    pub request: Duration,
    /// The `shutdown` response.
    pub shutdown: Duration,
    /// Graceful exit after `exit`, before the child is killed and reaped.
    pub exit: Duration,
}

impl Default for ExtensionLifecycle {
    fn default() -> Self {
        Self {
            spawn: Duration::from_secs(DEFAULT_SPAWN_SECONDS),
            initialize: Duration::from_secs(DEFAULT_INITIALIZE_SECONDS),
            ready: Duration::from_secs(DEFAULT_READY_SECONDS),
            request: Duration::from_secs(DEFAULT_REQUEST_SECONDS),
            shutdown: Duration::from_secs(DEFAULT_SHUTDOWN_SECONDS),
            exit: Duration::from_secs(DEFAULT_EXIT_SECONDS),
        }
    }
}

impl From<&crate::config::ExtensionLifecycleConfig> for ExtensionLifecycle {
    fn from(config: &crate::config::ExtensionLifecycleConfig) -> Self {
        Self {
            spawn: Duration::from_secs(config.spawn_seconds),
            initialize: Duration::from_secs(config.initialize_seconds),
            ready: Duration::from_secs(config.ready_seconds),
            request: Duration::from_secs(config.request_seconds),
            shutdown: Duration::from_secs(config.shutdown_seconds),
            exit: Duration::from_secs(config.exit_seconds),
        }
    }
}

/// Awaits one startup or registration step under a deadline and the caller's
/// cancellation token. Every failure names the extension and lifecycle stage.
async fn bounded_startup_step<T, F>(
    name: &str,
    stage: &str,
    deadline: Duration,
    cancel: &CancellationToken,
    step: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        result = tokio::time::timeout(deadline, step) => match result {
            Ok(inner) => inner.with_context(|| format!("extension {name} {stage}")),
            Err(_) => bail!(
                "extension {name} {stage} timed out after {}ms",
                deadline.as_millis()
            ),
        },
        () = cancel.cancelled() => bail!("extension {name} {stage} cancelled"),
    }
}

/// Kills a child that is still running and reaps it, bounded by `deadline`.
/// Every lifecycle failure path uses this so a timeout or cancellation never
/// leaves a zombie or orphan behind. `kill_on_drop` remains the last resort;
/// this is the deterministic path.
///
/// Waiting is attempted even when signalling fails: a child that exited in the
/// race after `try_wait` still has to be reaped.
async fn terminate_child(
    name: &str,
    stage: &str,
    child: &mut Child,
    deadline: Duration,
) -> Result<ExitStatus> {
    if let Some(status) = child
        .try_wait()
        .with_context(|| format!("extension {name} {stage}: reap"))?
    {
        return Ok(status);
    }
    let kill = child.start_kill();
    match tokio::time::timeout(deadline, child.wait()).await {
        // The wait succeeded: the child is reaped. A signalling error means it
        // had already exited in the `try_wait` race, so it is irrelevant.
        Ok(result) => result.with_context(|| format!("extension {name} {stage}: reap")),
        Err(_) => {
            kill.with_context(|| format!("extension {name} {stage}: kill"))?;
            bail!(
                "extension {name} {stage}: child did not exit after kill within {}ms",
                deadline.as_millis()
            )
        }
    }
}

/// Attaches a failed kill+reap to the lifecycle error that caused cleanup.
/// The original stage error is never hidden by a cleanup failure.
fn with_cleanup(error: anyhow::Error, cleanup: Result<ExitStatus>) -> anyhow::Error {
    match cleanup {
        Ok(_) => error,
        Err(cleanup) => anyhow!("{error:#}; {cleanup:#}"),
    }
}
pub struct FramedReader<R> {
    inner: BufReader<R>,
}
impl<R: AsyncRead + Unpin> FramedReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner: BufReader::new(inner),
        }
    }
    pub async fn read(&mut self) -> Result<RpcMessage> {
        read_frame(&mut self.inner).await
    }
}
pub struct FramedWriter<W> {
    inner: W,
}
impl<W: AsyncWrite + Unpin> FramedWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }
    pub async fn write(&mut self, message: &RpcMessage) -> Result<()> {
        let body = serde_json::to_vec(message)?;
        if body.len() > MAX_MESSAGE {
            bail!("extension message too large");
        }
        self.inner
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await?;
        self.inner.write_all(&body).await?;
        self.inner.flush().await?;
        Ok(())
    }
}
async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<RpcMessage> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            bail!("extension closed stream");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(v.trim().parse::<usize>()?);
        }
    }
    let n = length.ok_or_else(|| anyhow!("missing Content-Length"))?;
    if n > MAX_MESSAGE {
        bail!("extension frame exceeds {MAX_MESSAGE} bytes");
    }
    let mut body = vec![0; n];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[derive(Debug, Clone)]
pub struct RegisteredTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
#[derive(Debug, Clone, Default)]
pub struct ExtensionCapabilities {
    pub tools: Vec<RegisteredTool>,
    pub commands: Vec<String>,
    pub observe: Vec<String>,
    pub transform: Vec<String>,
    pub guard: Vec<String>,
    pub context_sources: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionGuardDecision {
    Allow,
    Deny(String),
    Ask(String),
}
pub struct ExtensionHost {
    name: String,
    child: Child,
    reader: FramedReader<ChildStdout>,
    writer: FramedWriter<ChildStdin>,
    next_id: u64,
    lifecycle: ExtensionLifecycle,
    pub capabilities: ExtensionCapabilities,
}
impl ExtensionHost {
    pub async fn start(
        name: String,
        command: &str,
        args: &[String],
        workspace: &str,
        sandbox: (&ExecutionBackend, &SandboxProfile),
        lifecycle: &ExtensionLifecycle,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        // Extension hosts always use the same sandbox as command execution.
        // Their tool arguments remain a cooperative boundary.
        let mut process = sandbox.0.fixed_command(
            sandbox.1,
            command,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
        let mut child = process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start extension {name}"))?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = terminate_child(&name, "startup", &mut child, lifecycle.exit).await;
                bail!("extension {name} stdin unavailable");
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = terminate_child(&name, "startup", &mut child, lifecycle.exit).await;
                bail!("extension {name} stdout unavailable");
            }
        };
        let mut host = Self {
            name,
            child,
            reader: FramedReader::new(stdout),
            writer: FramedWriter::new(stdin),
            next_id: 2,
            lifecycle: *lifecycle,
            capabilities: ExtensionCapabilities::default(),
        };
        if let Err(error) = host.handshake(workspace, cancel).await {
            // A partial or failed handshake leaves an unusable host. Kill and
            // reap it before surfacing the error so a silent extension can
            // never linger.
            let cleanup = host.terminate("startup cleanup").await;
            return Err(with_cleanup(error, cleanup));
        }
        Ok(host)
    }

    /// Bounded spawn/initialize plus registration/ready collection. The spawn
    /// deadline covers the first handshake write, the initialize deadline the
    /// response, and the ready deadline every registration message up to
    /// `ready`.
    async fn handshake(&mut self, workspace: &str, cancel: &CancellationToken) -> Result<()> {
        let name = self.name.clone();
        let initialize = request(
            1,
            "initialize",
            json!({"protocolVersion":EXTENSION_PROTOCOL_VERSION,"client":{"name":"latch","version":env!("CARGO_PKG_VERSION")},"workspace":workspace,"permissionContract":"cooperative-audit"}),
        );
        bounded_startup_step(&name, "spawn", self.lifecycle.spawn, cancel, async {
            self.writer.write(&initialize).await
        })
        .await?;
        let response = bounded_startup_step(
            &name,
            "initialize",
            self.lifecycle.initialize,
            cancel,
            self.reader.read(),
        )
        .await?;
        if response.id != Some(json!(1)) || response.error.is_some() {
            bail!("extension {name} initialize failed: {:?}", response.error);
        }
        let version = response
            .result
            .as_ref()
            .and_then(|v| v.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if version != EXTENSION_PROTOCOL_VERSION {
            bail!("extension {name} protocol mismatch: {version}");
        }
        bounded_startup_step(&name, "registration", self.lifecycle.ready, cancel, async {
            self.writer
                .write(&notification("initialized", json!({})))
                .await?;
            self.collect_registrations().await
        })
        .await?;
        Ok(())
    }
    async fn collect_registrations(&mut self) -> Result<()> {
        loop {
            let msg = self.reader.read().await?;
            match msg.method.as_deref() {
                Some("tool.register") => {
                    let p = msg
                        .params
                        .as_ref()
                        .ok_or_else(|| anyhow!("missing params"))?;
                    self.capabilities.tools.push(RegisteredTool {
                        name: string(p, "name")?.into(),
                        description: string(p, "description").unwrap_or("").into(),
                        input_schema: p
                            .get("inputSchema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type":"object"})),
                    });
                    self.respond_ok(msg.id).await?
                }
                Some("command.register") => {
                    self.capabilities
                        .commands
                        .push(string(msg.params.as_ref().unwrap_or(&Value::Null), "name")?.into());
                    self.respond_ok(msg.id).await?
                }
                Some("hook.observe") => {
                    self.capabilities
                        .observe
                        .push(string(msg.params.as_ref().unwrap_or(&Value::Null), "event")?.into());
                    self.respond_ok(msg.id).await?
                }
                Some("hook.transform") => {
                    self.capabilities.transform.push(
                        string(msg.params.as_ref().unwrap_or(&Value::Null), "structure")?.into(),
                    );
                    self.respond_ok(msg.id).await?
                }
                Some("hook.guard") => {
                    self.capabilities.guard.push(
                        string(msg.params.as_ref().unwrap_or(&Value::Null), "action")?.into(),
                    );
                    self.respond_ok(msg.id).await?
                }
                Some("context_source.register") => {
                    self.capabilities
                        .context_sources
                        .push(string(msg.params.as_ref().unwrap_or(&Value::Null), "name")?.into());
                    self.respond_ok(msg.id).await?
                }
                Some("ready") => break,
                Some(m) => bail!("unsupported registration method {m}"),
                None => bail!("expected extension registration"),
            }
        }
        Ok(())
    }
    async fn respond_ok(&mut self, id: Option<Value>) -> Result<()> {
        if let Some(id) = id {
            self.writer
                .write(&RpcMessage {
                    jsonrpc: "2.0".into(),
                    id: Some(id),
                    method: None,
                    params: None,
                    result: Some(json!({"ok":true})),
                    error: None,
                })
                .await?;
        }
        Ok(())
    }
    pub async fn execute_tool(
        &mut self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        if !self.capabilities.tools.iter().any(|t| t.name == name) {
            bail!("extension {} did not register tool {name}", self.name);
        }
        self.request_value(
            "tool.execute",
            json!({"name":name,"arguments":arguments}),
            cancel,
        )
        .await
    }
    pub async fn guard(
        &mut self,
        action: &str,
        payload: Value,
        cancel: &CancellationToken,
    ) -> Result<ExtensionGuardDecision> {
        if !self
            .capabilities
            .guard
            .iter()
            .any(|registered| registered == action || registered == "*")
        {
            return Ok(ExtensionGuardDecision::Allow);
        }
        let value = self
            .request_value(
                "hook.guard",
                json!({"action":action,"payload":payload}),
                cancel,
            )
            .await?;
        guard_decision(&value)
    }
    pub async fn transform(
        &mut self,
        structure: &str,
        value: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        if !self
            .capabilities
            .transform
            .iter()
            .any(|registered| registered == structure)
        {
            return Ok(value);
        }
        self.request_value(
            "hook.transform",
            json!({"structure":structure,"value":value}),
            cancel,
        )
        .await
    }
    pub async fn context(&mut self, cancel: &CancellationToken) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        for name in self.capabilities.context_sources.clone() {
            values.push(
                self.request_value("context_source.get", json!({"name":name}), cancel)
                    .await?,
            );
        }
        Ok(values)
    }
    async fn request_value(
        &mut self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let name = self.name.clone();
        let deadline = self.lifecycle.request;
        let exchange = request(id, method, params);
        let mut timed_out = false;
        let result = tokio::select! {
            // An unresponsive extension must not pin a cancelled run, and a
            // resolved request must still complete inside its deadline.
            result = tokio::time::timeout(deadline, async {
                self.writer.write(&exchange).await?;
                loop {
                    let msg = self.reader.read().await?;
                    if msg.id == Some(json!(id)) {
                        if let Some(error) = msg.error {
                            bail!("extension {name} {method} failed: {error}");
                        }
                        return msg
                            .result
                            .ok_or_else(|| anyhow!("extension {name} {method} returned no result"));
                    }
                }
            }) => match result {
                Ok(inner) => inner.with_context(|| format!("extension {name} {method}")),
                Err(_) => {
                    timed_out = true;
                    Ok(Value::Null)
                }
            },
            () = cancel.cancelled() => Err(anyhow!("extension {name} {method} cancelled")),
        };
        if timed_out {
            // A timeout means the extension stopped answering: kill and reap
            // it now so later calls fail fast instead of queueing behind a
            // request that will never complete.
            let timeout = anyhow!(
                "extension {name} {method} timed out after {}ms",
                deadline.as_millis()
            );
            let cleanup = self.terminate(method).await;
            return Err(with_cleanup(timeout, cleanup));
        }
        result
    }
    pub async fn observe(&mut self, event: &str, payload: Value) -> Result<()> {
        if self
            .capabilities
            .observe
            .iter()
            .any(|registered| registered == event || registered == "*")
        {
            let name = self.name.clone();
            let deadline = self.lifecycle.request;
            let notification =
                notification("hook.observe", json!({"event":event,"payload":payload}));
            match tokio::time::timeout(deadline, self.writer.write(&notification)).await {
                Ok(result) => {
                    result.with_context(|| format!("extension {name} observe {event}"))?
                }
                Err(_) => {
                    let timeout = anyhow!(
                        "extension {name} observe {event} timed out after {}ms",
                        deadline.as_millis()
                    );
                    let cleanup = self.terminate(&format!("observe {event}")).await;
                    return Err(with_cleanup(timeout, cleanup));
                }
            }
        }
        Ok(())
    }
    /// Bounded protocol shutdown: `shutdown` response, then `exit`, then a
    /// bounded graceful wait. Every failure kills and reaps the child before
    /// returning, so no shutdown error leaves a process behind.
    pub async fn shutdown(mut self) -> Result<()> {
        let result = match self.request_shutdown().await {
            Ok(()) => match self.request_exit().await {
                Ok(()) => self.await_exit().await,
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                // `await_exit` already killed and reaped on its own timeout;
                // this is a no-op then. Protocol failures before it still need
                // cleanup, and a failed cleanup stays visible.
                let cleanup = self.terminate("shutdown cleanup").await;
                Err(with_cleanup(error, cleanup))
            }
        }
    }
    async fn request_shutdown(&mut self) -> Result<()> {
        let name = self.name.clone();
        let deadline = self.lifecycle.shutdown;
        let id = self.next_id;
        let step = async {
            self.writer
                .write(&request(id, "shutdown", json!({})))
                .await?;
            let response = self.reader.read().await?;
            if response.id != Some(json!(id)) {
                bail!(
                    "extension {name} shutdown response id mismatch: expected {id}, got {:?}",
                    response.id
                );
            }
            if let Some(error) = response.error {
                bail!("extension {name} shutdown returned JSON-RPC error: {error}");
            }
            Ok(())
        };
        match tokio::time::timeout(deadline, step).await {
            Ok(result) => result.with_context(|| format!("extension {name} shutdown")),
            Err(_) => bail!(
                "extension {name} shutdown timed out after {}ms",
                deadline.as_millis()
            ),
        }
    }
    async fn request_exit(&mut self) -> Result<()> {
        let name = self.name.clone();
        let deadline = self.lifecycle.exit;
        let exit = notification("exit", json!({}));
        match tokio::time::timeout(deadline, self.writer.write(&exit)).await {
            Ok(result) => result.with_context(|| format!("extension {name} exit")),
            Err(_) => bail!(
                "extension {name} exit timed out after {}ms",
                deadline.as_millis()
            ),
        }
    }
    async fn await_exit(&mut self) -> Result<()> {
        match tokio::time::timeout(self.lifecycle.exit, self.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => bail!("extension {} exited with {status}", self.name),
            Ok(Err(error)) => Err(error).with_context(|| format!("extension {} exit", self.name)),
            Err(_) => {
                let cleanup = self.terminate("exit").await;
                if let Err(cleanup) = cleanup {
                    return Err(anyhow!(
                        "extension {} did not exit after {}ms; {cleanup:#}",
                        self.name,
                        self.lifecycle.exit.as_millis()
                    ));
                }
                bail!(
                    "extension {} did not exit after {}ms; killed and reaped",
                    self.name,
                    self.lifecycle.exit.as_millis()
                )
            }
        }
    }
    /// Kills the child if it is still running and reaps it, bounded by the
    /// exit deadline. Idempotent: a host that was already reaped returns its
    /// cached status without signalling again.
    async fn terminate(&mut self, stage: &str) -> Result<ExitStatus> {
        terminate_child(&self.name, stage, &mut self.child, self.lifecycle.exit).await
    }
}
fn request(id: u64, method: &str, params: Value) -> RpcMessage {
    RpcMessage {
        jsonrpc: "2.0".into(),
        id: Some(json!(id)),
        method: Some(method.into()),
        params: Some(params),
        result: None,
        error: None,
    }
}
fn notification(method: &str, params: Value) -> RpcMessage {
    RpcMessage {
        jsonrpc: "2.0".into(),
        id: None,
        method: Some(method.into()),
        params: Some(params),
        result: None,
        error: None,
    }
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing {key}"))
}

/// Parses a guard hook response. A malformed response must not silently
/// allow: an absent or non-string decision is an explicit error.
fn guard_decision(value: &Value) -> Result<ExtensionGuardDecision> {
    let decision = value
        .get("decision")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("extension guard response has no decision"))?;
    match decision {
        "allow" => Ok(ExtensionGuardDecision::Allow),
        "deny" => Ok(ExtensionGuardDecision::Deny(
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("extension denied action")
                .into(),
        )),
        "ask" => Ok(ExtensionGuardDecision::Ask(
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("extension requests approval")
                .into(),
        )),
        other => bail!("extension returned invalid guard decision {other}"),
    }
}

pub struct ExtensionRegistry {
    hosts: BTreeMap<String, ExtensionHost>,
    lifecycle: ExtensionLifecycle,
}
impl ExtensionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            hosts: BTreeMap::new(),
            lifecycle: ExtensionLifecycle::default(),
        }
    }
    /// Installs the central lifecycle policy used for later `add` calls.
    /// Already-running hosts keep the policy they started with.
    pub fn set_lifecycle(&mut self, lifecycle: ExtensionLifecycle) {
        self.lifecycle = lifecycle;
    }
    pub async fn add(
        &mut self,
        name: String,
        command: &str,
        args: &[String],
        workspace: &str,
        sandbox: (&ExecutionBackend, &SandboxProfile),
        cancel: &CancellationToken,
    ) -> Result<()> {
        let host = ExtensionHost::start(
            name.clone(),
            command,
            args,
            workspace,
            sandbox,
            &self.lifecycle,
            cancel,
        )
        .await?;
        self.hosts.insert(name, host);
        Ok(())
    }
    pub fn tools(&self) -> Vec<(String, RegisteredTool)> {
        self.hosts
            .iter()
            .flat_map(|(n, h)| h.capabilities.tools.iter().cloned().map(|t| (n.clone(), t)))
            .collect()
    }
    pub fn owner_for_tool(&self, tool: &str) -> Option<String> {
        self.hosts.iter().find_map(|(name, host)| {
            host.capabilities
                .tools
                .iter()
                .any(|registered| registered.name == tool)
                .then(|| name.clone())
        })
    }
    pub async fn execute(
        &mut self,
        extension: &str,
        tool: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.hosts
            .get_mut(extension)
            .ok_or_else(|| anyhow!("unknown extension {extension}"))?
            .execute_tool(tool, args, cancel)
            .await
    }
    pub async fn guard(
        &mut self,
        action: &str,
        payload: Value,
        cancel: &CancellationToken,
    ) -> Result<ExtensionGuardDecision> {
        for host in self.hosts.values_mut() {
            match host.guard(action, payload.clone(), cancel).await? {
                ExtensionGuardDecision::Allow => {}
                decision => return Ok(decision),
            }
        }
        Ok(ExtensionGuardDecision::Allow)
    }
    pub async fn transform(
        &mut self,
        structure: &str,
        mut value: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        for host in self.hosts.values_mut() {
            value = host.transform(structure, value, cancel).await?;
        }
        Ok(value)
    }
    pub async fn context(&mut self, cancel: &CancellationToken) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        for host in self.hosts.values_mut() {
            values.extend(host.context(cancel).await?);
        }
        Ok(values)
    }
    pub async fn shutdown_all(&mut self) -> Result<()> {
        let hosts = std::mem::take(&mut self.hosts);
        let mut errors = Vec::new();
        for (_, host) in hosts {
            if let Err(error) = host.shutdown().await {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("extension shutdown failures: {}", errors.join("; "))
        }
    }
    pub async fn observe(&mut self, event: &str, payload: Value) -> Result<()> {
        for host in self.hosts.values_mut() {
            host.observe(event, payload.clone()).await?;
        }
        Ok(())
    }
}
impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio::io::duplex;
    #[tokio::test]
    async fn framing_round_trip() {
        let (a, b) = duplex(4096);
        let (mut ar, mut aw) = tokio::io::split(a);
        let (mut br, mut bw) = tokio::io::split(b);
        let msg = request(7, "initialize", json!({"x":1}));
        let send = tokio::spawn(async move {
            FramedWriter::new(&mut aw).write(&msg).await.unwrap();
        });
        let got = FramedReader::new(&mut br).read().await.unwrap();
        send.await.unwrap();
        assert_eq!(got.method.as_deref(), Some("initialize"));
        let response = RpcMessage {
            jsonrpc: "2.0".into(),
            id: got.id,
            method: None,
            params: None,
            result: Some(json!({"ok":true})),
            error: None,
        };
        FramedWriter::new(&mut bw).write(&response).await.unwrap();
        let back = FramedReader::new(&mut ar).read().await.unwrap();
        assert_eq!(back.result.unwrap()["ok"], true);
    }

    #[test]
    fn guard_responses_are_fail_closed() {
        assert!(guard_decision(&json!({})).is_err(), "missing decision");
        assert!(guard_decision(&json!({"decision": 7})).is_err());
        assert!(guard_decision(&json!({"decision": "maybe"})).is_err());
        assert_eq!(
            guard_decision(&json!({"decision": "deny", "reason": "no"})).unwrap(),
            ExtensionGuardDecision::Deny("no".into())
        );
    }

    #[tokio::test]
    async fn host_registers_and_executes_external_tool() {
        use crate::sandbox::{Capability, CapabilitySet};

        let dir = tempfile::tempdir().unwrap();
        let Ok(runner) = ExecutionBackend::detect(dir.path()) else {
            return;
        };
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let profile = SandboxProfile::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            state_dir,
            [Capability::WorkspaceRead, Capability::NetworkAccess]
                .into_iter()
                .collect::<CapabilitySet>(),
        );
        let fixture = format!("{}/tests/fixtures/extension.py", env!("CARGO_MANIFEST_DIR"));
        let mut host = ExtensionHost::start(
            "fixture".into(),
            "python3",
            &[fixture],
            &dir.path().to_string_lossy(),
            (&runner, &profile),
            &ExtensionLifecycle::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(host.capabilities.tools[0].name, "fixture.echo");
        assert_eq!(host.capabilities.commands, ["fixture-about"]);
        assert_eq!(
            host.guard("tool.execute", json!({}), &CancellationToken::new())
                .await
                .unwrap(),
            ExtensionGuardDecision::Allow
        );
        assert_eq!(
            host.transform(
                "model_request",
                json!({"unchanged":true}),
                &CancellationToken::new()
            )
            .await
            .unwrap()["unchanged"],
            true
        );
        assert_eq!(
            host.context(&CancellationToken::new()).await.unwrap()[0]["content"],
            "fixture context"
        );
        let value = host
            .execute_tool(
                "fixture.echo",
                json!({"value":"hello"}),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(value["echoed"], "hello");
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sandboxed_extension_cannot_read_latch_credentials() {
        use crate::sandbox::{Capability, CapabilitySet};

        let dir = tempfile::tempdir().unwrap();
        let Ok(runner) = ExecutionBackend::detect(dir.path()) else {
            return;
        };
        let workspace = dir.path().join("workspace");
        let home = dir.path().join("home");
        let state_dir = workspace.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let normal = workspace.join("normal.txt");
        let secret = state_dir.join("secrets.toml");
        std::fs::write(&normal, "workspace data").unwrap();
        std::fs::write(&secret, "fake extension secret").unwrap();
        let capabilities = [
            Capability::WorkspaceRead,
            Capability::NetworkAccess,
            Capability::ExtensionExecution,
        ]
        .into_iter()
        .collect::<CapabilitySet>();
        let profile = SandboxProfile::new(workspace.clone(), home, state_dir, capabilities);
        let fixture = format!("{}/tests/fixtures/extension.py", env!("CARGO_MANIFEST_DIR"));
        let mut host = ExtensionHost::start(
            "fixture".into(),
            "python3",
            &[fixture],
            &workspace.to_string_lossy(),
            (&runner, &profile),
            &ExtensionLifecycle::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        for (path, expected) in [(&secret, false), (&normal, true)] {
            let result = host
                .execute_tool(
                    "fixture.echo",
                    json!({"read_path": path}),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(result["readable"], expected, "{}", path.display());
        }
        host.shutdown().await.unwrap();
    }

    /// Shared fake-extension harness for lifecycle-bound tests. Returns `None`
    /// when the host has no usable Bubblewrap sandbox.
    struct FakeExtension {
        _dir: tempfile::TempDir,
        workspace: PathBuf,
        /// Unique path passed to the fake; its command line is the needle the
        /// watcher uses to find the host PIDs of the whole process tree.
        marker: PathBuf,
        runner: ExecutionBackend,
        profile: SandboxProfile,
    }

    /// Host PIDs whose command line mentions `needle`. Processes in a child
    /// PID namespace are visible here under their initial-namespace PIDs.
    fn matching_pids(needle: &str) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut pids = Vec::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                continue;
            };
            if !cmdline.is_empty() && String::from_utf8_lossy(&cmdline).contains(needle) {
                pids.push(pid);
            }
        }
        pids
    }

    /// Records the sandbox and extension PIDs while a lifecycle operation
    /// runs. Asserting those exact PIDs are gone afterwards proves the child
    /// was reaped rather than left as a zombie (`/proc/<pid>` persists for a
    /// zombie).
    struct ProcessWatch {
        pids: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl ProcessWatch {
        fn start(fake: &FakeExtension) -> Self {
            let needle = fake.marker.to_string_lossy().into_owned();
            let pids = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let task = tokio::spawn({
                let pids = pids.clone();
                async move {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while Instant::now() < deadline {
                        for pid in matching_pids(&needle) {
                            let mut seen = pids.lock().expect("watch mutex poisoned");
                            if !seen.contains(&pid) {
                                seen.push(pid);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            });
            Self { pids, task }
        }

        /// Stops watching and returns every observed PID.
        fn finish(self) -> Vec<u32> {
            self.task.abort();
            self.pids.lock().expect("watch mutex poisoned").clone()
        }
    }

    /// Polls until every observed PID is gone from `/proc` (namespace teardown
    /// after the sandbox PID 1 is reaped is asynchronous but prompt).
    async fn assert_gone(pids: &[u32]) {
        assert!(!pids.is_empty(), "fake extension never ran");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let alive: Vec<u32> = pids
                .iter()
                .copied()
                .filter(|pid| PathBuf::from(format!("/proc/{pid}")).exists())
                .collect();
            if alive.is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "extension pids {alive:?} were not reaped"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn fake_extension() -> Option<FakeExtension> {
        use crate::sandbox::{Capability, CapabilitySet};

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let runner = ExecutionBackend::detect(&workspace).ok()?;
        let state_dir = workspace.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        // The fake needs a writable workspace only to stay a realistic
        // sandboxed extension; the watcher does not rely on the pid file.
        let capabilities = [
            Capability::WorkspaceRead,
            Capability::WorkspaceSourceWrite,
            Capability::NetworkAccess,
            Capability::ExtensionExecution,
        ]
        .into_iter()
        .collect::<CapabilitySet>();
        let profile = SandboxProfile::new(
            workspace.clone(),
            workspace.clone(),
            state_dir,
            capabilities,
        );
        let marker = workspace.join("extension.marker");
        Some(FakeExtension {
            _dir: dir,
            workspace,
            marker,
            runner,
            profile,
        })
    }

    impl FakeExtension {
        async fn start(
            &self,
            mode: &str,
            lifecycle: ExtensionLifecycle,
            cancel: &CancellationToken,
        ) -> Result<ExtensionHost> {
            let fixture = format!(
                "{}/tests/fixtures/lifecycle_extension.py",
                env!("CARGO_MANIFEST_DIR")
            );
            ExtensionHost::start(
                "fake".into(),
                "python3",
                &[
                    fixture,
                    mode.into(),
                    self.marker.to_string_lossy().into_owned(),
                ],
                &self.workspace.to_string_lossy(),
                (&self.runner, &self.profile),
                &lifecycle,
                cancel,
            )
            .await
        }
    }

    #[tokio::test]
    async fn silent_extension_is_reaped_when_initialize_times_out() {
        let Some(fake) = fake_extension() else { return };
        let lifecycle = ExtensionLifecycle {
            initialize: Duration::from_secs(2),
            ..ExtensionLifecycle::default()
        };
        let watch = ProcessWatch::start(&fake);
        let started = Instant::now();
        let error = match fake
            .start("silent-initialize", lifecycle, &CancellationToken::new())
            .await
        {
            Ok(_) => panic!("initialize must time out"),
            Err(error) => error,
        };
        let pids = watch.finish();
        let message = format!("{error:#}");
        assert!(message.contains("fake"), "{message}");
        assert!(message.contains("initialize"), "{message}");
        assert!(message.contains("timed out"), "{message}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout took {:?}",
            started.elapsed()
        );
        assert_gone(&pids).await;
    }

    #[tokio::test]
    async fn silent_extension_is_reaped_when_ready_never_arrives() {
        let Some(fake) = fake_extension() else { return };
        let lifecycle = ExtensionLifecycle {
            ready: Duration::from_millis(500),
            ..ExtensionLifecycle::default()
        };
        let watch = ProcessWatch::start(&fake);
        let error = match fake
            .start("silent-ready", lifecycle, &CancellationToken::new())
            .await
        {
            Ok(_) => panic!("registration must time out"),
            Err(error) => error,
        };
        let pids = watch.finish();
        let message = format!("{error:#}");
        assert!(message.contains("fake"), "{message}");
        assert!(message.contains("registration"), "{message}");
        assert!(message.contains("timed out"), "{message}");
        assert_gone(&pids).await;
    }

    #[tokio::test]
    async fn silent_extension_is_reaped_when_a_request_times_out() {
        let Some(fake) = fake_extension() else { return };
        let lifecycle = ExtensionLifecycle {
            request: Duration::from_millis(500),
            ..ExtensionLifecycle::default()
        };
        let mut host = fake
            .start("silent-rpc", lifecycle, &CancellationToken::new())
            .await
            .unwrap();
        let watch = ProcessWatch::start(&fake);
        let error = host
            .execute_tool(
                "lifecycle.echo",
                json!({"value":"x"}),
                &CancellationToken::new(),
            )
            .await
            .expect_err("tool.execute must time out");
        let pids = watch.finish();
        let message = format!("{error:#}");
        assert!(message.contains("fake"), "{message}");
        assert!(message.contains("tool.execute"), "{message}");
        assert!(message.contains("timed out"), "{message}");
        assert!(
            host.child.id().is_none(),
            "timed-out host must be killed and reaped"
        );
        assert_gone(&pids).await;
        // A dead host fails fast instead of hanging again.
        assert!(host.shutdown().await.is_err());
    }

    #[tokio::test]
    async fn silent_extension_is_reaped_when_shutdown_times_out() {
        let Some(fake) = fake_extension() else { return };
        let lifecycle = ExtensionLifecycle {
            shutdown: Duration::from_millis(500),
            ..ExtensionLifecycle::default()
        };
        let host = fake
            .start("silent-shutdown", lifecycle, &CancellationToken::new())
            .await
            .unwrap();
        let watch = ProcessWatch::start(&fake);
        let error = host.shutdown().await.expect_err("shutdown must time out");
        let pids = watch.finish();
        let message = format!("{error:#}");
        assert!(message.contains("fake"), "{message}");
        assert!(message.contains("shutdown"), "{message}");
        assert!(message.contains("timed out"), "{message}");
        assert_gone(&pids).await;
    }

    #[tokio::test]
    async fn shutdown_rejects_unrelated_response_and_json_rpc_error() {
        let Some(fake) = fake_extension() else { return };
        for (mode, expected) in [
            ("wrong-shutdown-id", "response id mismatch"),
            ("shutdown-error", "shutdown refused"),
        ] {
            let host = fake
                .start(
                    mode,
                    ExtensionLifecycle::default(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            let error = host.shutdown().await.expect_err(mode);
            let message = format!("{error:#}");
            assert!(message.contains(expected), "{mode}: {message}");
        }
    }

    #[tokio::test]
    async fn process_that_ignores_exit_is_killed_and_reaped() {
        let Some(fake) = fake_extension() else { return };
        let lifecycle = ExtensionLifecycle {
            exit: Duration::from_millis(500),
            ..ExtensionLifecycle::default()
        };
        let host = fake
            .start("ignore-exit", lifecycle, &CancellationToken::new())
            .await
            .unwrap();
        let watch = ProcessWatch::start(&fake);
        let error = host
            .shutdown()
            .await
            .expect_err("graceful exit must time out");
        let pids = watch.finish();
        let message = format!("{error:#}");
        assert!(message.contains("fake"), "{message}");
        assert!(message.contains("did not exit"), "{message}");
        assert!(message.contains("killed and reaped"), "{message}");
        assert_gone(&pids).await;
    }

    #[tokio::test]
    async fn cancelled_startup_kills_and_reaps_the_extension() {
        let Some(fake) = fake_extension() else { return };
        let cancel = CancellationToken::new();
        let needle = fake.marker.to_string_lossy().into_owned();
        let canceller = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                let deadline = Instant::now() + Duration::from_secs(5);
                while matching_pids(&needle).is_empty() {
                    assert!(Instant::now() < deadline, "fake extension never started");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                cancel.cancel();
            }
        });
        let watch = ProcessWatch::start(&fake);
        // Ten seconds of initialize budget: only the token can stop this.
        let started = Instant::now();
        let error = match fake
            .start("silent-initialize", ExtensionLifecycle::default(), &cancel)
            .await
        {
            Ok(_) => panic!("startup must be cancelled"),
            Err(error) => error,
        };
        canceller.await.unwrap();
        let pids = watch.finish();
        let message = format!("{error:#}");
        assert!(message.contains("fake"), "{message}");
        // Cancellation may land while the initialize request is being written
        // (the spawn stage) or while its response is awaited.
        assert!(
            message.contains("spawn") || message.contains("initialize"),
            "{message}"
        );
        assert!(message.contains("cancelled"), "{message}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancellation took {:?}",
            started.elapsed()
        );
        assert_gone(&pids).await;
    }

    #[tokio::test]
    async fn cancelled_request_leaves_a_healthy_extension_running() {
        let Some(fake) = fake_extension() else { return };
        let mut host = fake
            .start(
                "silent-rpc",
                ExtensionLifecycle::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = match host
            .execute_tool("lifecycle.echo", json!({"value":"x"}), &cancel)
            .await
        {
            Ok(_) => panic!("cancelled request must fail"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("fake"), "{message}");
        assert!(message.contains("tool.execute"), "{message}");
        assert!(message.contains("cancelled"), "{message}");
        assert!(
            host.child.id().is_some(),
            "cancellation must not kill a healthy host"
        );
        // The host stays usable after a cancelled RPC.
        host.shutdown().await.unwrap();
    }
}
