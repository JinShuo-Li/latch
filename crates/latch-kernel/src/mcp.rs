//! Bounded MCP client. External metadata is never a capability grant.
use crate::{execution::ExecutionBackend, sandbox::SandboxProfile};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use futures::StreamExt;
use latch_protocol::ToolDefinition;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;

pub const MODERN: &str = "2026-07-28";
const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const LIMIT: usize = 4 * 1024 * 1024;
const MAX_TOOLS: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    #[serde(flatten)]
    pub transport: McpTransportConfig,
}
fn enabled() -> bool {
    true
}
fn timeout() -> u64 {
    120
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    StreamableHttp {
        url: String,
        #[serde(default)]
        bearer_token_env: Option<String>,
    },
}
impl McpServerConfig {
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty()
            || self.name.len() > 32
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            bail!("MCP server name must be 1–32 ASCII letters, digits or hyphens");
        }
        if self.timeout_seconds == 0 || self.timeout_seconds > 3600 {
            bail!("MCP timeout_seconds must be 1–3600");
        }
        match &self.transport {
            McpTransportConfig::Stdio { command, .. } if command.trim().is_empty() => {
                bail!("MCP stdio command is empty")
            }
            McpTransportConfig::StreamableHttp { url, .. } => {
                let url = reqwest::Url::parse(url).context("invalid MCP URL")?;
                if !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    bail!("MCP URL cannot contain credentials or a fragment");
                }
                let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
                if url.scheme() != "https" && !(url.scheme() == "http" && local) {
                    bail!("MCP requires HTTPS except on loopback");
                }
            }
            _ => {}
        }
        Ok(())
    }
}
struct StdioTransport {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    pending: Vec<u8>,
}
struct HttpTransport {
    client: reqwest::Client,
    url: String,
    token: Option<String>,
    session: Option<String>,
}
enum Transport {
    Stdio(Box<StdioTransport>),
    Http(HttpTransport),
}

struct StdioReconnect {
    config: McpServerConfig,
    backend: ExecutionBackend,
    profile: SandboxProfile,
}

pub struct McpClient {
    pub name: String,
    pub version: String,
    transport: Transport,
    next_id: u64,
    deadline: Duration,
    tools: BTreeMap<String, (String, ToolDefinition)>,
    failed: bool,
    reconnect: Option<Box<StdioReconnect>>,
}
impl McpClient {
    pub async fn connect(
        config: &McpServerConfig,
        sandbox: (&ExecutionBackend, &SandboxProfile),
        cancel: &CancellationToken,
    ) -> Result<Self> {
        Self::connect_scoped(config, sandbox, cancel, cfg!(windows)).await
    }
    async fn connect_scoped(
        config: &McpServerConfig,
        sandbox: (&ExecutionBackend, &SandboxProfile),
        cancel: &CancellationToken,
        short_lived: bool,
    ) -> Result<Self> {
        let mut client = Self::connect_active(config, sandbox, cancel).await?;
        if short_lived && matches!(client.transport, Transport::Stdio(_)) {
            // Windows recovery deliberately permits one live transaction per
            // user. Never pin its lock while the agent is idle or using tools.
            client.shutdown().await?;
            client.failed = false;
            client.reconnect = Some(Box::new(StdioReconnect {
                config: config.clone(),
                backend: sandbox.0.clone(),
                profile: sandbox.1.clone(),
            }));
        }
        Ok(client)
    }
    async fn connect_active(
        config: &McpServerConfig,
        sandbox: (&ExecutionBackend, &SandboxProfile),
        cancel: &CancellationToken,
    ) -> Result<Self> {
        config.validate()?;
        let transport = match &config.transport {
            McpTransportConfig::Stdio { command, args } => {
                let mut command = sandbox.0.fixed_command(
                    sandbox.1,
                    command,
                    &args.iter().map(String::as_str).collect::<Vec<_>>(),
                )?;
                let mut child = command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .context("start sandboxed MCP server")?;
                let input = child.stdin.take().context("MCP stdin unavailable")?;
                let output = child.stdout.take().context("MCP stdout unavailable")?;
                Transport::Stdio(Box::new(StdioTransport {
                    child,
                    input: Some(input),
                    output: BufReader::new(output),
                    pending: Vec::new(),
                }))
            }
            McpTransportConfig::StreamableHttp {
                url,
                bearer_token_env,
            } => {
                let token = bearer_token_env
                    .as_ref()
                    .map(|name| {
                        std::env::var(name).with_context(|| {
                            format!("MCP credential environment variable {name} is unavailable")
                        })
                    })
                    .transpose()?;
                Transport::Http(HttpTransport {
                    client: reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .no_proxy()
                        .build()?,
                    url: url.clone(),
                    token,
                    session: None,
                })
            }
        };
        let mut client = Self {
            name: config.name.clone(),
            version: MODERN.into(),
            transport,
            next_id: 1,
            deadline: Duration::from_secs(config.timeout_seconds),
            tools: BTreeMap::new(),
            failed: false,
            reconnect: None,
        };
        let startup = async {
            client.negotiate(cancel).await?;
            client.discover(cancel).await
        };
        let result = tokio::select! { r = tokio::time::timeout(Duration::from_secs(config.timeout_seconds), startup) => r.context("MCP startup timed out").and_then(|r| r), () = cancel.cancelled() => Err(anyhow!("MCP startup cancelled")) };
        if let Err(error) = result {
            let _ = client.shutdown().await;
            return Err(error.context(format!("MCP {} startup", config.name)));
        }
        Ok(client)
    }
    fn message(&mut self, method: &str, mut params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        if self.version == MODERN {
            params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion": self.version, "io.modelcontextprotocol/clientInfo": {"name":"latch", "version":env!("CARGO_PKG_VERSION")}, "io.modelcontextprotocol/clientCapabilities":{}});
        }
        json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
    }
    async fn negotiate(&mut self, cancel: &CancellationToken) -> Result<()> {
        let probe = self.message("server/discover", json!({}));
        let probe_result = tokio::select! {
            r = tokio::time::timeout(self.deadline.min(Duration::from_secs(3)), self.exchange(&probe, None)) => r.ok().and_then(Result::ok),
            () = cancel.cancelled() => bail!("MCP discovery cancelled"),
        };
        if let Some(response) = probe_result.as_ref() {
            if response.get("result").is_some() {
                let versions = response
                    .pointer("/result/supportedVersions")
                    .and_then(Value::as_array)
                    .context("MCP discovery missing supportedVersions")?;
                if !versions.iter().any(|v| v.as_str() == Some(MODERN)) {
                    bail!("MCP server has no supported modern version");
                }
                return Ok(());
            }
            if modern_error(response) {
                if response.pointer("/error/code").and_then(Value::as_i64) == Some(-32022)
                    && response
                        .pointer("/error/data/supported")
                        .and_then(Value::as_array)
                        .is_some_and(|v| v.iter().any(|v| v.as_str() == Some(MODERN)))
                {
                    return Ok(());
                }
                bail!("MCP modern negotiation failed: {}", response["error"]);
            }
        }
        self.version = LEGACY[0].into();
        let init = self.message("initialize", json!({"protocolVersion":self.version,"capabilities":{},"clientInfo":{"name":"latch","version":env!("CARGO_PKG_VERSION")}}));
        let value = rpc_result(self.exchange(&init, None).await?)?;
        let version = value["protocolVersion"]
            .as_str()
            .context("MCP initialize missing version")?;
        if !LEGACY.contains(&version) {
            bail!("unsupported MCP protocol version {version}");
        }
        self.version = version.into();
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }
    async fn discover(&mut self, cancel: &CancellationToken) -> Result<()> {
        let mut cursor = None;
        let mut cursors = BTreeSet::new();
        loop {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |c| json!({"cursor":c}));
            let page = self.request("tools/list", params, None, cancel).await?;
            for tool in page["tools"]
                .as_array()
                .context("MCP tools/list missing tools")?
            {
                if self.tools.len() >= MAX_TOOLS {
                    bail!("MCP tool catalog exceeds 512 entries");
                }
                let original = tool["name"].as_str().context("MCP tool missing name")?;
                if original.is_empty() || original.len() > 128 {
                    bail!("invalid MCP tool name");
                }
                // Hex encoding is reversible, collision-free and provider-safe.
                let name = format!(
                    "mcp_{}_{}",
                    self.name,
                    hex::encode(sha2::Sha256::digest(original.as_bytes()))[..24].to_owned()
                );
                if name.len() > 64 {
                    bail!("MCP tool name too long for provider: {original}");
                }
                let schema = tool
                    .get("inputSchema")
                    .cloned()
                    .context("MCP tool missing inputSchema")?;
                if schema["type"] != "object" {
                    bail!("MCP tool inputSchema must be an object schema");
                }
                if self.version == MODERN
                    && matches!(self.transport, Transport::Http(_))
                    && let Err(error) = parameter_headers(&schema, &json!({}))
                {
                    tracing::warn!("rejecting MCP tool {original}: {error}");
                    continue;
                }
                let definition = ToolDefinition {
                    name: name.clone(),
                    description: format!(
                        "MCP {} / {} (external tool; annotations are not authorization). {}",
                        self.name,
                        original,
                        tool["description"].as_str().unwrap_or("")
                    ),
                    input_schema: schema,
                };
                if self
                    .tools
                    .insert(name, (original.into(), definition))
                    .is_some()
                {
                    bail!("duplicate MCP tool name");
                }
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            match &cursor {
                None => return Ok(()),
                Some(c) if !cursors.insert(c.clone()) => bail!("MCP pagination cursor repeated"),
                _ => {}
            }
        }
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|(_, d)| d.clone()).collect()
    }
    pub fn owns(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
    pub async fn execute(
        &mut self,
        name: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        if self.failed {
            bail!("MCP server disconnected; start a new session to reconnect");
        }
        if !self.owns(name) || !args.is_object() {
            bail!("unregistered MCP tool or invalid arguments");
        }
        if let Some(reconnect) = &self.reconnect {
            let mut active = match Self::connect_active(
                &reconnect.config,
                (&reconnect.backend, &reconnect.profile),
                cancel,
            )
            .await
            {
                Ok(active) => active,
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            };
            active.deadline = self.deadline;
            let same_catalog = active.version == self.version
                && serde_json::to_value(active.definitions())?
                    == serde_json::to_value(self.definitions())?;
            let result = if same_catalog {
                active.execute_active(name, args, cancel).await
            } else {
                self.failed = true;
                Err(anyhow!(
                    "MCP catalog changed; start a new session before executing tools"
                ))
            };
            self.failed |= active.failed;
            let cleanup = active.shutdown().await;
            if cleanup.is_err() {
                self.failed = true;
            }
            return match result {
                Ok(value) => cleanup.map(|_| value),
                Err(error) => Err(error),
            };
        }
        self.execute_active(name, args, cancel).await
    }
    async fn execute_active(
        &mut self,
        name: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let (original, definition) = self
            .tools
            .get(name)
            .context("unregistered MCP tool")?
            .clone();
        if !args.is_object() {
            bail!("MCP arguments must be an object");
        }
        self.request(
            "tools/call",
            json!({"name":original, "arguments":args}),
            Some(&definition.input_schema),
            cancel,
        )
        .await
        .and_then(validate_tool_result)
    }
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        schema: Option<&Value>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        if self.failed {
            bail!("MCP server disconnected; start a new session to reconnect");
        }
        let message = self.message(method, params);
        let result = tokio::select! {
            r = tokio::time::timeout(self.deadline, self.exchange(&message, schema)) => r.context("MCP request timed out").and_then(|r| r),
            () = cancel.cancelled() => Err(anyhow!("MCP request cancelled")),
        };
        match result {
            Ok(value) => rpc_result(value),
            Err(error) => {
                // No automatic retry: a disconnected tool may already have acted.
                if matches!(self.transport, Transport::Stdio(_)) || self.version != MODERN {
                    let _ = tokio::time::timeout(Duration::from_millis(250), self.notify("notifications/cancelled", json!({"requestId":message["id"],"reason":"client cancelled or timed out"}))).await;
                }
                self.failed = true;
                let _ = self.shutdown().await;
                Err(error)
            }
        }
    }
    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let message = json!({"jsonrpc":"2.0","method":method,"params":params});
        match &mut self.transport {
            Transport::Stdio(t) => write_stdio(t, &message).await,
            Transport::Http(t) => {
                http_post(t, &self.version, &message, None).await?;
                Ok(())
            }
        }
    }
    async fn exchange(&mut self, message: &Value, schema: Option<&Value>) -> Result<Value> {
        match &mut self.transport {
            Transport::Stdio(t) => {
                write_stdio(t, message).await?;
                loop {
                    let value = read_stdio(t).await?;
                    validate_rpc(&value)?;
                    if value.get("method").is_some() {
                        if let Some(id) = value.get("id") {
                            // We advertise no sampling, elicitation or roots capability.
                            write_stdio(t, &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client capability not supported"}})).await?;
                        }
                        continue;
                    }
                    if value.get("id") == message.get("id")
                        || (value.get("id").is_none() && modern_error(&value))
                    {
                        return Ok(value);
                    }
                    // Late replies from the bounded modern probe are harmless.
                }
            }
            Transport::Http(t) => http_post(t, &self.version, message, schema).await,
        }
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        self.failed = true;
        match &mut self.transport {
            Transport::Stdio(t) => {
                t.input.take();
                if tokio::time::timeout(Duration::from_millis(500), t.child.wait())
                    .await
                    .is_err()
                {
                    t.child.start_kill()?;
                    tokio::time::timeout(Duration::from_secs(3), t.child.wait())
                        .await
                        .context("MCP process cleanup timed out")??;
                }
            }
            Transport::Http(t) => {
                if let Some(session) = t.session.take() {
                    let mut req = t
                        .client
                        .delete(&t.url)
                        .header("Mcp-Session-Id", session)
                        .header("MCP-Protocol-Version", &self.version);
                    if let Some(token) = &t.token {
                        req = req.bearer_auth(token);
                    }
                    let response = tokio::time::timeout(Duration::from_secs(3), req.send())
                        .await
                        .context("MCP session cleanup timed out")?
                        .context("MCP session cleanup failed")?;
                    if !response.status().is_success()
                        && !matches!(response.status().as_u16(), 404 | 405)
                    {
                        bail!("MCP session cleanup HTTP {}", response.status());
                    }
                }
            }
        }
        Ok(())
    }
}
fn validate_tool_result(value: Value) -> Result<Value> {
    if !value.get("content").is_some_and(Value::is_array)
        || value.get("isError").is_some_and(|v| !v.is_boolean())
    {
        bail!("MCP tools/call returned an invalid result");
    }
    Ok(value)
}

fn modern_error(value: &Value) -> bool {
    matches!(
        value.pointer("/error/code").and_then(Value::as_i64),
        Some(-32022..=-32020)
    )
}
fn validate_rpc(value: &Value) -> Result<()> {
    if value["jsonrpc"] != "2.0" || !value.is_object() {
        bail!("invalid MCP JSON-RPC envelope");
    }
    Ok(())
}
fn rpc_result(value: Value) -> Result<Value> {
    validate_rpc(&value)?;
    if let Some(error) = value.get("error") {
        bail!("MCP RPC error: {error}");
    }
    let result = value
        .get("result")
        .cloned()
        .context("MCP response missing result")?;
    if result.get("inputRequests").is_some()
        || result
            .get("resultType")
            .is_some_and(|kind| kind != "complete")
    {
        bail!("MCP tool requested unsupported client interaction");
    }
    Ok(result)
}
async fn write_stdio(t: &mut StdioTransport, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > LIMIT {
        bail!("MCP outgoing message exceeds limit");
    }
    bytes.push(b'\n');
    let input = t.input.as_mut().context("MCP input closed")?;
    input.write_all(&bytes).await?;
    input.flush().await?;
    Ok(())
}
async fn read_stdio(t: &mut StdioTransport) -> Result<Value> {
    loop {
        let bytes = t.output.fill_buf().await?;
        if bytes.is_empty() {
            bail!("MCP server closed stdout");
        }
        let end = bytes.iter().position(|b| *b == b'\n');
        let n = end.map_or(bytes.len(), |n| n + 1);
        if t.pending.len() + n > LIMIT {
            bail!("MCP message exceeds 4 MiB");
        }
        t.pending.extend_from_slice(&bytes[..n]);
        t.output.consume(n);
        if end.is_some() {
            return serde_json::from_slice(&std::mem::take(&mut t.pending))
                .context("invalid MCP JSON");
        }
    }
}
async fn http_post(
    t: &mut HttpTransport,
    version: &str,
    message: &Value,
    schema: Option<&Value>,
) -> Result<Value> {
    let body = serde_json::to_vec(message)?;
    if body.len() > LIMIT {
        bail!("MCP outgoing message exceeds limit");
    }
    let mut request = t
        .client
        .post(&t.url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("MCP-Protocol-Version", version)
        .body(body);
    if let Some(token) = &t.token {
        request = request.bearer_auth(token);
    }
    if let Some(session) = &t.session {
        request = request.header("Mcp-Session-Id", session);
    }
    if version == MODERN {
        request = request.header(
            "Mcp-Method",
            message["method"].as_str().context("missing MCP method")?,
        );
        if let Some(name) = message.pointer("/params/name").and_then(Value::as_str) {
            request = request.header("Mcp-Name", encode_header(name));
        }
        if let Some(schema) = schema {
            for (name, value) in parameter_headers(schema, &message["params"]["arguments"])? {
                request = request.header(name, value);
            }
        }
    }
    let response = request
        .send()
        .await
        .map_err(|_| anyhow!("MCP HTTP request failed (check endpoint and credentials)"))?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        bail!("MCP HTTP authorization failed ({status})");
    }
    if version != MODERN
        && let Some(session) = response.headers().get("Mcp-Session-Id")
    {
        let value = session.to_str()?;
        if value.is_empty()
            || value.len() > 1024
            || !value.bytes().all(|b| (0x21..=0x7e).contains(&b))
        {
            bail!("invalid MCP session id");
        }
        t.session = Some(value.into());
    }
    if message.get("id").is_none() && status.as_u16() == 202 {
        return Ok(Value::Null);
    }
    let sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next() == Some("text/event-stream"));
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    let mut total = 0usize;
    let mut data = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("MCP HTTP response stream failed")?;
        total = total.saturating_add(chunk.len());
        if total > LIMIT {
            bail!("MCP HTTP response exceeds 4 MiB");
        }
        bytes.extend_from_slice(&chunk);
        if sse {
            while let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                let line = String::from_utf8(bytes.drain(..=end).collect())?;
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() && !data.is_empty() {
                    let value: Value = serde_json::from_str(&data)?;
                    data.clear();
                    validate_rpc(&value)?;
                    if (value.get("id") == message.get("id")
                        || (value.get("id").is_none() && modern_error(&value)))
                        && value.get("method").is_none()
                    {
                        return Ok(value);
                    }
                    if value.get("id").is_some() && value.get("method").is_some() {
                        bail!("MCP server requested an unsupported client capability");
                    }
                } else if let Some(line) = line.strip_prefix("data:") {
                    data.push_str(line.strip_prefix(' ').unwrap_or(line));
                    data.push('\n');
                }
            }
        }
    }
    if sse {
        bail!("MCP SSE ended without a matching response");
    }
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("MCP HTTP {status}: invalid JSON response"))?;
    validate_rpc(&value)?;
    if value.get("id") != message.get("id") && !(value.get("id").is_none() && modern_error(&value))
    {
        bail!("MCP HTTP response id mismatch");
    }
    if !status.is_success() && value.get("error").is_none() {
        bail!("MCP HTTP {status}");
    }
    Ok(value)
}
fn encode_header(value: &str) -> String {
    if value.trim() == value
        && value
            .bytes()
            .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
        && !(value.starts_with("=?base64?") && value.ends_with("?="))
    {
        value.into()
    } else {
        format!(
            "=?base64?{}?=",
            base64::engine::general_purpose::STANDARD.encode(value)
        )
    }
}
fn parameter_headers(schema: &Value, args: &Value) -> Result<BTreeMap<String, String>> {
    fn visit(
        schema: &Value,
        args: &Value,
        reachable: bool,
        names: &mut BTreeSet<String>,
        headers: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        if let Some(name) = schema.get("x-mcp-header") {
            let name = name.as_str().context("x-mcp-header must be a string")?;
            if !reachable
                || name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                || !names.insert(name.to_ascii_lowercase())
                || !matches!(
                    schema["type"].as_str(),
                    Some("string" | "integer" | "boolean")
                )
            {
                bail!("invalid x-mcp-header annotation");
            }
            if !args.is_null() {
                let text = match (schema["type"].as_str(), args) {
                    (Some("string"), Value::String(s)) => s.clone(),
                    (Some("boolean"), Value::Bool(b)) => b.to_string(),
                    (Some("integer"), Value::Number(n))
                        if n.as_i64().is_some_and(|n| {
                            (-9007199254740991..=9007199254740991).contains(&n)
                        }) =>
                    {
                        n.to_string()
                    }
                    _ => bail!("invalid MCP header parameter type"),
                };
                headers.insert(format!("Mcp-Param-{name}"), encode_header(&text));
            }
        }
        if let Some(obj) = schema.as_object() {
            for (key, value) in obj {
                if key == "properties" {
                    if let Some(properties) = value.as_object() {
                        for (name, value) in properties {
                            visit(value, &args[name], reachable, names, headers)?;
                        }
                    }
                } else if key != "x-mcp-header" {
                    visit(value, &Value::Null, false, names, headers)?;
                }
            }
        } else if let Some(array) = schema.as_array() {
            for value in array {
                visit(value, &Value::Null, false, names, headers)?;
            }
        }
        Ok(())
    }
    let mut headers = BTreeMap::new();
    visit(schema, args, true, &mut BTreeSet::new(), &mut headers)?;
    Ok(headers)
}
#[derive(Default)]
pub struct McpRegistry {
    clients: BTreeMap<String, McpClient>,
}
impl McpRegistry {
    pub async fn add(
        &mut self,
        config: &McpServerConfig,
        sandbox: (&ExecutionBackend, &SandboxProfile),
        cancel: &CancellationToken,
    ) -> Result<()> {
        if self.clients.contains_key(&config.name) {
            bail!("duplicate MCP server {}", config.name);
        }
        let client = McpClient::connect(config, sandbox, cancel).await?;
        self.clients.insert(config.name.clone(), client);
        Ok(())
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.clients
            .values()
            .flat_map(McpClient::definitions)
            .collect()
    }
    pub fn owns(&self, tool: &str) -> bool {
        self.clients.values().any(|c| c.owns(tool))
    }
    pub async fn execute(
        &mut self,
        tool: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.clients
            .values_mut()
            .find(|c| c.owns(tool))
            .context("unknown MCP tool")?
            .execute(tool, args, cancel)
            .await
    }
    pub fn status(&self) -> String {
        self.clients
            .values()
            .map(|c| {
                format!(
                    "{}: {} protocol {}, {} tools",
                    c.name,
                    if c.failed {
                        "disconnected"
                    } else if c.reconnect.is_some() {
                        "ready (per-call stdio)"
                    } else {
                        "connected"
                    },
                    c.version,
                    c.tools.len()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        for client in self.clients.values_mut() {
            if let Err(e) = client.shutdown().await {
                errors.push(e.to_string());
            }
        }
        if !errors.is_empty() {
            bail!("MCP cleanup: {}", errors.join("; "));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_security() {
        for url in [
            "http://example.com/mcp",
            "https://user:pass@example.com/mcp",
            "https://example.com/#secret",
            "file:///etc/passwd",
        ] {
            assert!(
                McpServerConfig {
                    name: "sample".into(),
                    enabled: true,
                    timeout_seconds: 1,
                    transport: McpTransportConfig::StreamableHttp {
                        url: url.into(),
                        bearer_token_env: None
                    }
                }
                .validate()
                .is_err()
            );
        }
        let c: McpServerConfig = toml::from_str(
            "name='sample'\ntransport='stdio'\ncommand='python3'\nargs=['server.py']",
        )
        .unwrap();
        assert!(c.validate().is_ok());
    }
    #[test]
    fn headers_cannot_inject_or_collide() {
        assert_eq!(encode_header("x\r\ny"), "=?base64?eA0KeQ==?=");
        assert_ne!(encode_header("=?base64?literal?="), "=?base64?literal?=");
        let schema = json!({"type":"object","properties":{"region":{"type":"string","x-mcp-header":"Region"}}});
        assert_eq!(
            parameter_headers(&schema, &json!({"region":"east"})).unwrap()["Mcp-Param-Region"],
            "east"
        );
        assert!(parameter_headers(&json!({"oneOf":[schema]}), &json!({})).is_err());
        assert!(parameter_headers(&json!({"properties":{"a":{"type":"string","x-mcp-header":"X"},"b":{"type":"string","x-mcp-header":"x"}}}), &json!({})).is_err());
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::sandbox::{Capability, CapabilitySet};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn mcp_sandboxed_stdio_modern_and_legacy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("state")).unwrap();
        let fixture = dir.path().join("mcp.py");
        std::fs::write(&fixture, include_str!("../tests/fixtures/mcp.py")).unwrap();
        let backend = ExecutionBackend::detect(dir.path()).expect("mandatory sandbox must work");
        let profile = SandboxProfile::new(
            dir.path().into(),
            dirs::home_dir().unwrap_or_else(|| dir.path().into()),
            dir.path().join("state"),
            [
                Capability::WorkspaceRead,
                Capability::NetworkAccess,
                Capability::ExtensionExecution,
            ]
            .into_iter()
            .collect::<CapabilitySet>(),
        );
        for legacy in [false, true] {
            for short_lived in [false, true] {
                let mut args = vec![fixture.to_string_lossy().into_owned()];
                if legacy {
                    args.push("--legacy".into());
                }
                let config = McpServerConfig {
                    name: "fixture".into(),
                    enabled: true,
                    timeout_seconds: 60,
                    transport: McpTransportConfig::Stdio {
                        command: if cfg!(windows) { "python" } else { "python3" }.into(),
                        args,
                    },
                };
                let mut client = McpClient::connect_scoped(
                    &config,
                    (&backend, &profile),
                    &CancellationToken::new(),
                    short_lived,
                )
                .await
                .unwrap();
                assert_eq!(client.version, if legacy { "2024-11-05" } else { MODERN });
                let name = client.definitions()[0].name.clone();
                let value = client
                    .execute(&name, json!({"hello":"世界"}), &CancellationToken::new())
                    .await
                    .unwrap();
                assert!(
                    value["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("hello")
                );
                assert_eq!(
                    client
                        .execute(&name, json!({"fail":true}), &CancellationToken::new())
                        .await
                        .unwrap()["isError"],
                    true
                );
                let cancel = CancellationToken::new();
                if legacy {
                    cancel.cancel();
                } else {
                    client.deadline = Duration::from_millis(20);
                }
                assert!(
                    client
                        .execute(&name, json!({"sleep":true}), &cancel)
                        .await
                        .is_err()
                );
                assert!(client.failed);
                assert!(
                    client
                        .execute(&name, json!({}), &CancellationToken::new())
                        .await
                        .is_err()
                );
                client.shutdown().await.unwrap();
            }
        }
    }

    async fn http_fixture(sse: bool, legacy: bool) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (headers, message) = loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .and_then(|l| l.parse::<usize>().ok())
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break (
                                headers,
                                serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + length])
                                    .unwrap_or(Value::Null),
                            );
                        }
                    }
                };
                if headers.starts_with("delete") {
                    socket
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .await
                        .unwrap();
                    return;
                }
                let method = message["method"].as_str().unwrap();
                let mut response = json!({"jsonrpc":"2.0","id":message["id"]});
                let mut status = "200 OK";
                if method == "server/discover" && legacy {
                    response["error"] = json!({"code":-32601,"message":"legacy"});
                    status = "400 Bad Request";
                } else {
                    response["result"] = match method {
                        "server/discover" => json!({"supportedVersions":[MODERN]}),
                        "initialize" => {
                            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"test","version":"1"}})
                        }
                        "tools/list" => {
                            json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})
                        }
                        "tools/call" => {
                            if !legacy {
                                assert!(headers.contains("mcp-method: tools/call"));
                                assert!(headers.contains("mcp-name: echo"));
                                assert_eq!(
                                    message["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
                                    MODERN
                                );
                            } else {
                                assert!(headers.contains("mcp-session-id: session-1"));
                            }
                            json!({"content":[{"type":"text","text":"ok"}]})
                        }
                        "notifications/initialized" => {
                            status = "202 Accepted";
                            Value::Null
                        }
                        _ => panic!("unexpected method {method}"),
                    };
                }
                let body = if status.starts_with("202") {
                    String::new()
                } else if sse {
                    format!(": heartbeat\r\ndata: {}\r\n\r\n", response)
                } else {
                    response.to_string()
                };
                let content_type = if sse {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let session = if method == "initialize" {
                    "Mcp-Session-Id: session-1\r\n"
                } else {
                    ""
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{session}Connection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                // Deliberately split UTF-8 / SSE framing across chunks.
                for chunk in body.as_bytes().chunks(7) {
                    socket.write_all(chunk).await.unwrap();
                }
            }
        });
        (url, task)
    }
    #[tokio::test]
    async fn mcp_http_json_sse_and_legacy_sessions() {
        for sse in [false, true] {
            for legacy in [false, true] {
                let (url, task) = http_fixture(sse, legacy).await;
                // HTTP transport itself never launches a process; test directly without a backend.
                let mut client = McpClient {
                    name: "http".into(),
                    version: MODERN.into(),
                    transport: Transport::Http(HttpTransport {
                        client: reqwest::Client::builder().no_proxy().build().unwrap(),
                        url,
                        token: None,
                        session: None,
                    }),
                    next_id: 1,
                    deadline: Duration::from_secs(5),
                    tools: BTreeMap::new(),
                    failed: false,
                    reconnect: None,
                };
                let cancel = CancellationToken::new();
                client.negotiate(&cancel).await.unwrap();
                client.discover(&cancel).await.unwrap();
                let name = client.definitions()[0].name.clone();
                assert_eq!(
                    client.execute(&name, json!({}), &cancel).await.unwrap()["content"][0]["text"],
                    "ok"
                );
                client.shutdown().await.unwrap();
                task.abort();
            }
        }
    }
    #[tokio::test]
    async fn mcp_rpc_rejects_interactions_and_malformed_envelopes() {
        assert!(rpc_result(json!({"jsonrpc":"1.0","result":{}})).is_err());
        assert!(rpc_result(json!({"jsonrpc":"2.0","result":{"resultType":"input_required","requestState":"pending"}})).is_err());
        for code in [-32020, -32021, -32022] {
            assert!(modern_error(&json!({"error":{"code":code}})));
        }
        assert!(!modern_error(&json!({"error":{"code":-32601}})));
        assert!(validate_tool_result(json!({})).is_err());
        assert!(validate_tool_result(json!({"content":[],"isError":"false"})).is_err());
        assert!(validate_tool_result(json!({"content":[],"isError":true})).is_ok());
        assert!(rpc_result(json!({"jsonrpc":"2.0","result":{"inputRequests":[{}]}})).is_err());
        assert!(rpc_result(json!({"jsonrpc":"2.0","error":{"code":-1}})).is_err());
    }
}

#[cfg(test)]
mod negotiation_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn modern_capability_errors_without_ids_never_fall_back() {
        for sse in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/mcp", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                let value = json!({"jsonrpc":"2.0","error":{"code":-32021,"message":"Client capability required","data":{"requiredCapabilities":{"elicitation":{}}}}});
                let body = if sse {
                    format!("data: {value}\n\n")
                } else {
                    value.to_string()
                };
                let content_type = if sse {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            });
            let mut client = McpClient {
                name: "probe".into(),
                version: MODERN.into(),
                transport: Transport::Http(HttpTransport {
                    client: reqwest::Client::builder().no_proxy().build().unwrap(),
                    url,
                    token: None,
                    session: None,
                }),
                next_id: 1,
                deadline: Duration::from_secs(1),
                tools: BTreeMap::new(),
                failed: false,
                reconnect: None,
            };
            let error = client
                .negotiate(&CancellationToken::new())
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("modern negotiation failed"),
                "{error:#}"
            );
            assert_eq!(client.version, MODERN);
            assert_eq!(client.next_id, 2);
            task.await.unwrap();
        }
    }
}
