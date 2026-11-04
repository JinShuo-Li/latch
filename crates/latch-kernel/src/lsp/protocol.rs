//! Bounded Content-Length JSON-RPC, deliberately independent of extension/MCP
//! schemas. Server requests have an explicit read-only allowlist.
use super::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

pub(super) async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Value> {
    let mut header = Vec::new();
    let mut length = None;
    loop {
        let mut line = Vec::new();
        // read_until alone allocates without a bound on malicious headers.
        let mut limited = (&mut *reader).take(8193 - header.len() as u64);
        if limited.read_until(b'\n', &mut line).await? == 0 {
            bail!("LSP closed its output");
        }
        header.extend_from_slice(&line);
        if header.len() > 8192 || !line.ends_with(b"\n") {
            bail!("LSP header exceeds 8 KiB");
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        let line = std::str::from_utf8(&line).context("invalid LSP header")?;
        if let Some((key, value)) = line.trim_end().split_once(':') {
            if key.eq_ignore_ascii_case("Content-Length") {
                if length.is_some() {
                    bail!("duplicate LSP Content-Length");
                }
                let size: usize = value.trim().parse().context("invalid LSP Content-Length")?;
                if size == 0 || size > FRAME_BYTES {
                    bail!("LSP frame must be 1 byte–4 MiB");
                }
                length = Some(size);
            }
        } else {
            bail!("malformed LSP header");
        }
    }
    let length = length.context("missing LSP Content-Length")?;
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .context("truncated LSP frame")?;
    let value: Value = serde_json::from_slice(&bytes).context("invalid LSP JSON")?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !value.is_object() {
        bail!("LSP requires JSON-RPC 2.0 objects");
    }
    Ok(value)
}
impl Client {
    pub(super) async fn start(
        config: &LspServerConfig,
        scope: &Scope,
        backend: &ExecutionBackend,
        profile: &SandboxProfile,
    ) -> Result<Self> {
        let args = config.args.iter().map(String::as_str).collect::<Vec<_>>();
        let mut command = backend
            .fixed_command(profile, &config.command, &args)
            .context("prepare sandboxed LSP server")?;
        let mut child = command.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true).spawn().with_context(|| format!("start LSP {}: install/configure its language server; use search/read_file if unavailable", config.name))?;
        let input = child.stdin.take().context("missing LSP stdin")?;
        let output = BufReader::new(child.stdout.take().context("missing LSP stdout")?);
        let root_uri = reqwest::Url::from_directory_path(&scope.workspace)
            .map_err(|()| anyhow::anyhow!("invalid LSP workspace URI"))?
            .to_string();
        let mut client = Self {
            child,
            input,
            output,
            next_id: 0,
            next_version: 0,
            capabilities: Value::Null,
            settings: config.settings.clone(),
            root_uri,
            documents: BTreeMap::new(),
            diagnostics: BTreeMap::new(),
        };
        let initialized = client.rpc("initialize", json!({
            "processId":null, "clientInfo":{"name":"latch","version":env!("CARGO_PKG_VERSION")},
            "rootUri":client.root_uri, "workspaceFolders":[{"uri":client.root_uri,"name":"workspace"}],
            "initializationOptions":config.initialization_options,
            "capabilities":{
                "general":{"positionEncodings":["utf-16"]},
                "workspace":{"configuration":true,"workspaceFolders":true,"applyEdit":false,"didChangeWatchedFiles":{"dynamicRegistration":false}},
                "textDocument":{"synchronization":{"dynamicRegistration":false,"didSave":true},"documentSymbol":{"hierarchicalDocumentSymbolSupport":true},"definition":{"linkSupport":true},"hover":{"contentFormat":["plaintext","markdown"]},"diagnostic":{"dynamicRegistration":false},"publishDiagnostics":{"versionSupport":true}},
                "window":{"workDoneProgress":true}
            }
        })).await.with_context(|| format!("initialize LSP {} failed: verify the installed server command/configuration; use search/read_file if unavailable", config.name))?;
        client.capabilities = initialized
            .get("capabilities")
            .cloned()
            .context("LSP initialize omitted capabilities")?;
        if client
            .capabilities
            .get("positionEncoding")
            .and_then(Value::as_str)
            .is_some_and(|s| s != "utf-16")
        {
            bail!("LSP server selected unsupported position encoding (requires UTF-16)");
        }
        let sync = &client.capabilities["textDocumentSync"];
        let sync_kind = sync.as_u64().or_else(|| sync["change"].as_u64());
        if !matches!(sync_kind, Some(1 | 2)) || (sync.is_object() && sync["openClose"] != true) {
            bail!(
                "LSP server must support open/close and full or incremental text synchronization"
            );
        }
        client.notify("initialized", json!({})).await?;
        if !config.settings.is_null() {
            client
                .notify(
                    "workspace/didChangeConfiguration",
                    json!({"settings":config.settings}),
                )
                .await?;
        }
        Ok(client)
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        let body = serde_json::to_vec(&value)?;
        if body.len() > FRAME_BYTES {
            bail!("LSP outgoing frame exceeds 4 MiB");
        }
        self.input
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await?;
        self.input.write_all(&body).await?;
        self.input.flush().await?;
        Ok(())
    }
    pub(super) async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }
    pub(super) async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id = self.next_id.checked_add(1).context("LSP id overflow")?;
        let id = self.next_id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        for _ in 0..2048 {
            let message = self.message().await?;
            if message.get("method").is_none()
                && message.get("id").and_then(Value::as_u64) == Some(id)
            {
                if let Some(error) = message.get("error") {
                    bail!(
                        "LSP {method}: {}",
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("server error")
                            .chars()
                            .take(1000)
                            .collect::<String>()
                    );
                }
                return message
                    .get("result")
                    .cloned()
                    .context("LSP response omitted result");
            }
        }
        bail!("LSP notification/request flood (2048 frames)")
    }
    pub(super) async fn message(&mut self) -> Result<Value> {
        let message = read_frame(&mut self.output).await?;
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if let Some(id) = message.get("id") {
                let params = &message["params"];
                let result = match method {
                    "workspace/configuration" => Some(Value::Array(
                        params["items"]
                            .as_array()
                            .context("invalid LSP configuration request")?
                            .iter()
                            .take(64)
                            .map(|item| {
                                let mut value = &self.settings;
                                for part in item["section"]
                                    .as_str()
                                    .unwrap_or("")
                                    .split('.')
                                    .filter(|s| !s.is_empty())
                                {
                                    value = &value[part];
                                }
                                value.clone()
                            })
                            .collect(),
                    )),
                    "workspace/workspaceFolders" => {
                        Some(json!([{"uri":self.root_uri,"name":"workspace"}]))
                    }
                    "workspace/applyEdit" => Some(
                        json!({"applied":false,"failureReason":"Latch LSP is read-only; use guarded patch/write tools"}),
                    ),
                    "window/workDoneProgress/create" | "window/showMessageRequest" => {
                        Some(Value::Null)
                    }
                    _ => None,
                };
                let response = match result {
                    Some(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
                    None => {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported by read-only Latch LSP client"}})
                    }
                };
                self.send(response).await?;
            } else if method == "textDocument/publishDiagnostics" {
                let params = &message["params"];
                if let Some(uri) = params["uri"]
                    .as_str()
                    .filter(|uri| self.documents.contains_key(*uri))
                {
                    let diagnostics = query::diagnostic_items(&params["diagnostics"])?;
                    self.diagnostics.insert(uri.to_owned(), json!({"version":params["version"],"items":diagnostics,"total":params["diagnostics"].as_array().map_or(0, Vec::len)}));
                }
            }
        }
        Ok(message)
    }
    pub(super) async fn close(&mut self) {
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            self.rpc("shutdown", Value::Null),
        )
        .await;
        let _ = tokio::time::timeout(Duration::from_millis(100), self.notify("exit", Value::Null))
            .await;
        crate::execution::kill_sandbox_children(self.child.id());
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await;
    }
}
