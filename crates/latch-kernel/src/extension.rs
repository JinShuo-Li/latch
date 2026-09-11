use anyhow::{Context, Result, anyhow, bail};
use latch_protocol::{EXTENSION_PROTOCOL_VERSION, RpcMessage};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::process::Stdio;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const MAX_MESSAGE: usize = 16 * 1024 * 1024;
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
pub struct ExtensionHost {
    name: String,
    child: Child,
    reader: FramedReader<ChildStdout>,
    writer: FramedWriter<ChildStdin>,
    next_id: u64,
    pub capabilities: ExtensionCapabilities,
}
impl ExtensionHost {
    pub async fn start(
        name: String,
        command: &str,
        args: &[String],
        workspace: &str,
    ) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start extension {name}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("extension stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("extension stdout unavailable"))?;
        let mut host = Self {
            name,
            child,
            reader: FramedReader::new(stdout),
            writer: FramedWriter::new(stdin),
            next_id: 2,
            capabilities: ExtensionCapabilities::default(),
        };
        host.writer.write(&request(1,"initialize",json!({"protocolVersion":EXTENSION_PROTOCOL_VERSION,"client":{"name":"latch","version":env!("CARGO_PKG_VERSION")},"workspace":workspace,"permissionContract":"cooperative-audit"}))).await?;
        let response = host.reader.read().await?;
        if response.id != Some(json!(1)) || response.error.is_some() {
            bail!("extension initialize failed: {:?}", response.error);
        }
        let version = response
            .result
            .as_ref()
            .and_then(|v| v.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if version != EXTENSION_PROTOCOL_VERSION {
            bail!("extension protocol mismatch: {version}");
        }
        host.writer
            .write(&notification("initialized", json!({})))
            .await?;
        host.collect_registrations().await?;
        Ok(host)
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
    pub async fn execute_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        if !self.capabilities.tools.iter().any(|t| t.name == name) {
            bail!("extension {} did not register tool {name}", self.name);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.writer
            .write(&request(
                id,
                "tool.execute",
                json!({"name":name,"arguments":arguments}),
            ))
            .await?;
        loop {
            let msg = self.reader.read().await?;
            if msg.id == Some(json!(id)) {
                if let Some(e) = msg.error {
                    bail!("extension tool failed: {e}");
                }
                return msg
                    .result
                    .ok_or_else(|| anyhow!("extension returned no result"));
            }
        }
    }
    pub async fn shutdown(mut self) -> Result<()> {
        let id = self.next_id;
        self.writer
            .write(&request(id, "shutdown", json!({})))
            .await?;
        let _ = self.reader.read().await?;
        self.writer.write(&notification("exit", json!({}))).await?;
        let status = self.child.wait().await?;
        if !status.success() {
            bail!("extension exited with {status}");
        }
        Ok(())
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

pub struct ExtensionRegistry {
    hosts: BTreeMap<String, ExtensionHost>,
}
impl ExtensionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            hosts: BTreeMap::new(),
        }
    }
    pub async fn add(
        &mut self,
        name: String,
        command: &str,
        args: &[String],
        workspace: &str,
    ) -> Result<()> {
        let host = ExtensionHost::start(name.clone(), command, args, workspace).await?;
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
    pub async fn execute(&mut self, extension: &str, tool: &str, args: Value) -> Result<Value> {
        self.hosts
            .get_mut(extension)
            .ok_or_else(|| anyhow!("unknown extension {extension}"))?
            .execute_tool(tool, args)
            .await
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
}
impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn host_registers_and_executes_external_tool() {
        let fixture = format!("{}/tests/fixtures/extension.py", env!("CARGO_MANIFEST_DIR"));
        let mut host = ExtensionHost::start("fixture".into(), "python3", &[fixture], "/tmp")
            .await
            .unwrap();
        assert_eq!(host.capabilities.tools[0].name, "fixture.echo");
        assert_eq!(host.capabilities.commands, ["fixture-about"]);
        let value = host
            .execute_tool("fixture.echo", json!({"value":"hello"}))
            .await
            .unwrap();
        assert_eq!(value["echoed"], "hello");
        host.shutdown().await.unwrap();
    }
}
