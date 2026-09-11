use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;
use latch_protocol::{ModelRequest, ModelResponse, StreamEvent, ToolCall, Usage};
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// User-Agent sent with every provider request. Keep in sync with the workspace version.
pub const USER_AGENT: &str = "latch/0.1.1";
const OPENCODE_GO_BASE: &str = "https://opencode.ai/zen/go";
const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

pub type StreamSink = Arc<dyn Fn(StreamEvent) + Send + Sync>;
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse>;
}

pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    session_id: Option<Uuid>,
}
impl OpenAiProvider {
    #[must_use]
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').into(),
            api_key,
            model,
            session_id: None,
        }
    }
    /// Tags requests with the durable Latch session id. OpenCode Go endpoints
    /// receive it as `x-opencode-session`; other OpenAI-compatible servers are
    /// unaffected. Because a resumed session reloads the same stored UUID, the
    /// header stays stable for the lifetime of the session.
    #[must_use]
    pub fn with_session(mut self, session_id: Uuid) -> Self {
        self.session_id = Some(session_id);
        self
    }
    /// Headers attached to every model request.
    fn request_headers(&self) -> HeaderMap {
        let mut headers = user_agent_headers();
        if is_opencode_go_endpoint(&self.base_url)
            && let Some(session) = &self.session_id
            && let Ok(value) = HeaderValue::from_str(&session.to_string())
        {
            headers.insert(OPENCODE_SESSION_HEADER, value);
        }
        headers
    }
}
#[async_trait]
impl ModelProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }
    fn model(&self) -> &str {
        &self.model
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let body = openai_request(&request, &self.model);
        let response = checked_response(
            "openai-compatible",
            self.client
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(&self.api_key)
                .headers(self.request_headers())
                .json(&body)
                .send()
                .await?,
        )
        .await?;
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        let mut stop = "stop".to_string();
        let mut usage = None;
        loop {
            let next = tokio::select! {()=cancel.cancelled()=>bail!("model request cancelled"),v=bytes.next()=>v};
            let Some(chunk) = next else { break };
            for data in decoder.push(&chunk?) {
                if data == "[DONE]" {
                    continue;
                }
                let v: Value = serde_json::from_str(&data)?;
                if let Some(error) = v.get("error") {
                    bail!("OpenAI-compatible stream error: {error}");
                }
                if let Some(u) = v.get("usage") {
                    usage = Some(Usage {
                        input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
                        output_tokens: u
                            .get("completion_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    });
                }
                let Some(choice) = v
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                else {
                    continue;
                };
                if let Some(s) = choice.get("finish_reason").and_then(Value::as_str) {
                    stop = s.into();
                }
                let delta = &choice["delta"];
                if let Some(t) = delta.get("content").and_then(Value::as_str) {
                    text.push_str(t);
                    sink(StreamEvent::TextDelta(t.into()));
                }
                if let Some(t) = delta.get("reasoning_content").and_then(Value::as_str) {
                    reasoning.push_str(t);
                }
                if let Some(tc) = delta.get("tool_calls").and_then(Value::as_array) {
                    for c in tc {
                        let i = c.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let e = calls.entry(i).or_default();
                        if let Some(id) = c.get("id").and_then(Value::as_str) {
                            e.0 = id.into();
                        }
                        if let Some(n) = c.pointer("/function/name").and_then(Value::as_str) {
                            e.1.push_str(n);
                        }
                        if let Some(a) = c.pointer("/function/arguments").and_then(Value::as_str) {
                            e.2.push_str(a);
                        }
                    }
                }
            }
        }
        let tool_calls = calls
            .into_values()
            .map(|(id, name, args)| {
                Ok(ToolCall {
                    id,
                    name,
                    arguments: serde_json::from_str(&args)
                        .with_context(|| format!("invalid tool arguments: {args}"))?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let result = ModelResponse {
            text,
            tool_calls,
            stop_reason: stop,
            usage,
            reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
        };
        sink(StreamEvent::Completed(result.clone()));
        Ok(result)
    }
}

pub struct AnthropicProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
}
impl AnthropicProvider {
    #[must_use]
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').into(),
            api_key,
            model,
        }
    }
}
#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }
    fn model(&self) -> &str {
        &self.model
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let response = checked_response(
            "anthropic",
            self.client
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .headers(user_agent_headers())
                .json(&anthropic_request(&request, &self.model))
                .send()
                .await?,
        )
        .await?;
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut text = String::new();
        let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
        let mut stop = "end_turn".into();
        let mut usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
        };
        loop {
            let next = tokio::select! {()=cancel.cancelled()=>bail!("model request cancelled"),v=bytes.next()=>v};
            let Some(chunk) = next else { break };
            for data in decoder.push(&chunk?) {
                let v: Value = serde_json::from_str(&data)?;
                match v.get("type").and_then(Value::as_str) {
                    Some("error") => {
                        bail!("Anthropic stream error: {}", v.get("error").unwrap_or(&v))
                    }
                    Some("message_start") => {
                        usage.input_tokens = v
                            .pointer("/message/usage/input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                    }
                    Some("content_block_start") => {
                        if v.pointer("/content_block/type").and_then(Value::as_str)
                            == Some("tool_use")
                        {
                            let i = v["index"].as_u64().unwrap_or(0);
                            calls.insert(
                                i,
                                (
                                    v.pointer("/content_block/id")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .into(),
                                    v.pointer("/content_block/name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .into(),
                                    String::new(),
                                ),
                            );
                        }
                    }
                    Some("content_block_delta") => {
                        match v.pointer("/delta/type").and_then(Value::as_str) {
                            Some("text_delta") => {
                                let t = v
                                    .pointer("/delta/text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                text.push_str(t);
                                sink(StreamEvent::TextDelta(t.into()));
                            }
                            Some("input_json_delta") => {
                                if let Some(c) = calls.get_mut(&v["index"].as_u64().unwrap_or(0)) {
                                    c.2.push_str(
                                        v.pointer("/delta/partial_json")
                                            .and_then(Value::as_str)
                                            .unwrap_or(""),
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("message_delta") => {
                        if let Some(s) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                            stop = s.into();
                        }
                        usage.output_tokens = v
                            .pointer("/usage/output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                    }
                    _ => {}
                }
            }
        }
        let tool_calls = calls
            .into_values()
            .map(|(id, name, args)| {
                Ok(ToolCall {
                    id,
                    name,
                    arguments: serde_json::from_str(&args)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let result = ModelResponse {
            text,
            tool_calls,
            stop_reason: stop,
            usage: Some(usage),
            reasoning_content: None,
        };
        sink(StreamEvent::Completed(result.clone()));
        Ok(result)
    }
}

#[derive(Default)]
pub struct FakeProvider {
    responses: Mutex<VecDeque<ModelResponse>>,
    model: String,
}
impl FakeProvider {
    #[must_use]
    pub fn scripted(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            model: "scripted".into(),
        }
    }
}
#[async_trait]
impl ModelProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }
    fn model(&self) -> &str {
        &self.model
    }
    async fn stream(
        &self,
        _: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        if cancel.is_cancelled() {
            bail!("cancelled")
        }
        let r = self
            .responses
            .lock()
            .map_err(|_| anyhow!("fake provider lock poisoned"))?
            .pop_front()
            .ok_or_else(|| anyhow!("fake provider exhausted"))?;
        for ch in r.text.as_bytes().chunks(8) {
            let s = String::from_utf8_lossy(ch).into_owned();
            sink(StreamEvent::TextDelta(s));
        }
        for call in &r.tool_calls {
            sink(StreamEvent::ToolCallDelta(call.clone()));
        }
        sink(StreamEvent::Completed(r.clone()));
        Ok(r)
    }
}

fn user_agent_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );
    headers
}
fn is_opencode_go_endpoint(base_url: &str) -> bool {
    match base_url
        .trim_end_matches('/')
        .strip_prefix(OPENCODE_GO_BASE)
    {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// Inspects the HTTP status before streaming. On failure the provider body is
/// retained so callers get an actionable diagnostic instead of a bare status
/// code. Only the status and body are surfaced; request headers (and therefore
/// credentials) are never included.
async fn checked_response(
    provider: &str,
    response: reqwest::Response,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    bail!(
        "{provider} request failed with HTTP {}: {}",
        status.as_u16(),
        bound_error_body(&body)
    )
}

fn bound_error_body(body: &str) -> String {
    const LIMIT: usize = 4_000;
    let body = body.trim();
    if body.len() <= LIMIT {
        return body.to_string();
    }
    let mut end = LIMIT;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated {} bytes]", &body[..end], body.len() - end)
}

/// Serializes a provider-independent request into the OpenAI chat-completions
/// wire format. Assistant tool calls and `role: "tool"` results are preserved
/// structurally, and reasoning is replayed verbatim for tool-call turns.
#[must_use]
pub fn openai_request(request: &ModelRequest, model: &str) -> Value {
    let mut messages = vec![json!({"role":"system","content":request.system})];
    messages.extend(request.messages.iter().map(openai_message));
    json!({"model":model,"messages":messages,"tools":request.tools.iter().map(|t|json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}})).collect::<Vec<_>>(),"stream":true,"stream_options":{"include_usage":true}})
}

fn openai_message(message: &latch_protocol::ModelMessage) -> Value {
    match message.role.as_str() {
        "assistant" => {
            let mut value = json!({"role":"assistant","content":message.content});
            if !message.tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    message
                        .tool_calls
                        .iter()
                        .map(openai_tool_call)
                        .collect::<Vec<_>>(),
                );
                if let Some(reasoning) = &message.reasoning_content {
                    value["reasoning_content"] = Value::String(reasoning.clone());
                }
            }
            value
        }
        "tool" => json!({
            "role":"tool",
            "tool_call_id": message.tool_call_id.clone().unwrap_or_default(),
            "content": message.content,
        }),
        role => json!({"role": role, "content": message.content}),
    }
}

fn openai_tool_call(call: &ToolCall) -> Value {
    json!({
        "id": call.id,
        "type": "function",
        "function": {
            "name": call.name,
            "arguments": serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into()),
        }
    })
}

/// Serializes a provider-independent request into the Anthropic Messages wire
/// format. Tool calls become `tool_use` blocks and tool results become
/// `tool_result` blocks; OpenAI-only fields such as `reasoning_content` are
/// deliberately not emitted.
#[must_use]
pub fn anthropic_request(request: &ModelRequest, model: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < request.messages.len() {
        let message = &request.messages[index];
        match message.role.as_str() {
            "assistant" => {
                let mut blocks = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(json!({"type":"text","text":message.content}));
                }
                for call in &message.tool_calls {
                    blocks.push(json!({
                        "type":"tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type":"text","text":""}));
                }
                messages.push(json!({"role":"assistant","content":blocks}));
                index += 1;
            }
            "tool" => {
                let mut blocks = Vec::new();
                while index < request.messages.len() && request.messages[index].role == "tool" {
                    blocks.push(json!({
                        "type":"tool_result",
                        "tool_use_id": request.messages[index]
                            .tool_call_id
                            .clone()
                            .unwrap_or_default(),
                        "content": request.messages[index].content,
                    }));
                    index += 1;
                }
                messages.push(json!({"role":"user","content":blocks}));
            }
            role => {
                messages.push(json!({"role": role, "content": message.content}));
                index += 1;
            }
        }
    }
    json!({"model":model,"max_tokens":8192,"system":request.system,"messages":messages,"tools":request.tools.iter().map(|t|json!({"name":t.name,"description":t.description,"input_schema":t.input_schema})).collect::<Vec<_>>(),"stream":true})
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}
impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut out = vec![];
        loop {
            let delimiter = match (
                find_bytes(&self.buffer, b"\n\n"),
                find_bytes(&self.buffer, b"\r\n\r\n"),
            ) {
                (Some(a), Some(b)) if a <= b => Some((a, 2)),
                (Some(a), _) => Some((a, 2)),
                (_, Some(b)) => Some((b, 4)),
                _ => None,
            };
            let Some((i, delimiter_len)) = delimiter else {
                break;
            };
            let frame = String::from_utf8_lossy(&self.buffer[..i]).replace('\r', "");
            self.buffer.drain(..i + delimiter_len);
            let data = frame
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                out.push(data);
            }
        }
        out
    }
}
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use latch_protocol::{ModelMessage, ToolDefinition};
    #[test]
    fn converts_provider_requests() {
        let r = ModelRequest {
            system: "s".into(),
            messages: vec![ModelMessage::text("user", "hi")],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "r".into(),
                input_schema: json!({"type":"object"}),
            }],
        };
        let o = openai_request(&r, "m");
        assert_eq!(o["messages"][0]["role"], "system");
        assert_eq!(o["tools"][0]["function"]["name"], "read");
        let a = anthropic_request(&r, "m");
        assert_eq!(a["system"], "s");
        assert_eq!(a["tools"][0]["name"], "read");
    }

    #[test]
    fn openai_preserves_tool_calls_results_and_reasoning() {
        let r = ModelRequest {
            system: "s".into(),
            messages: vec![
                ModelMessage::text("user", "inspect"),
                ModelMessage {
                    role: "assistant".into(),
                    content: "working".into(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "a.txt"}),
                    }],
                    tool_call_id: None,
                    reasoning_content: Some("step by step".into()),
                },
                ModelMessage {
                    role: "tool".into(),
                    content: "contents".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("call-1".into()),
                    reasoning_content: None,
                },
            ],
            tools: vec![],
        };
        let body = openai_request(&r, "m");
        let assistant = &body["messages"][2];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["reasoning_content"], "step by step");
        assert_eq!(assistant["tool_calls"][0]["id"], "call-1");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.txt\"}"
        );
        let tool = &body["messages"][3];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call-1");
        assert_eq!(tool["content"], "contents");
    }

    #[test]
    fn anthropic_uses_content_blocks_and_drops_openai_fields() {
        let r = ModelRequest {
            system: "s".into(),
            messages: vec![
                ModelMessage::text("user", "inspect"),
                ModelMessage {
                    role: "assistant".into(),
                    content: "working".into(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "a.txt"}),
                    }],
                    tool_call_id: None,
                    reasoning_content: Some("secret reasoning".into()),
                },
                ModelMessage {
                    role: "tool".into(),
                    content: "contents".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("call-1".into()),
                    reasoning_content: None,
                },
            ],
            tools: vec![],
        };
        let body = anthropic_request(&r, "m");
        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "text");
        assert_eq!(assistant["content"][1]["type"], "tool_use");
        assert_eq!(assistant["content"][1]["id"], "call-1");
        assert_eq!(assistant["content"][1]["input"]["path"], "a.txt");
        assert!(!assistant.to_string().contains("reasoning_content"));
        let tool = &body["messages"][2];
        assert_eq!(tool["role"], "user");
        assert_eq!(tool["content"][0]["type"], "tool_result");
        assert_eq!(tool["content"][0]["tool_use_id"], "call-1");
    }

    #[test]
    fn error_diagnostics_include_status_and_body_without_headers() {
        let body = bound_error_body("{\"error\":{\"message\":\"bad request\"}}");
        assert!(body.contains("bad request"));
        let long = "x".repeat(10_000);
        let bounded = bound_error_body(&long);
        assert!(bounded.len() < long.len() && bounded.contains("truncated"));
    }
    #[test]
    fn decodes_fragmented_sse() {
        let mut d = SseDecoder::default();
        assert!(d.push(b"data: {\"a\":").is_empty());
        assert_eq!(d.push(b"1}\n\n"), ["{\"a\":1}"]);
        assert_eq!(d.push(b"data: ok\r\n\r\n"), ["ok"]);
    }
    #[test]
    fn generic_endpoints_send_only_the_user_agent() {
        let provider = OpenAiProvider::new(
            "https://api.openai.com/v1/".into(),
            "key".into(),
            "m".into(),
        )
        .with_session(Uuid::new_v4());
        let headers = provider.request_headers();
        assert_eq!(
            headers.get(reqwest::header::USER_AGENT).unwrap(),
            USER_AGENT
        );
        assert!(headers.get(OPENCODE_SESSION_HEADER).is_none());
    }
    #[test]
    fn opencode_go_session_header_is_stable_for_the_session_lifetime() {
        let session = Uuid::new_v4();
        let base = "https://opencode.ai/zen/go/models/gpt-5".to_string();
        let provider =
            OpenAiProvider::new(base.clone(), "key".into(), "m".into()).with_session(session);
        let first = provider.request_headers();
        assert_eq!(
            first.get(OPENCODE_SESSION_HEADER).unwrap(),
            &session.to_string()
        );
        // Stable across requests within a process...
        assert_eq!(
            provider.request_headers().get(OPENCODE_SESSION_HEADER),
            first.get(OPENCODE_SESSION_HEADER)
        );
        // ...and across provider rebuilds that resume the same durable session.
        let resumed = OpenAiProvider::new(base, "key".into(), "m".into()).with_session(session);
        assert_eq!(
            resumed.request_headers().get(OPENCODE_SESSION_HEADER),
            first.get(OPENCODE_SESSION_HEADER)
        );
        assert_eq!(
            first.get(reqwest::header::USER_AGENT).unwrap(),
            "latch/0.1.1"
        );
    }
    #[test]
    fn detects_opencode_go_endpoints_at_path_boundaries() {
        for url in [
            "https://opencode.ai/zen/go",
            "https://opencode.ai/zen/go/",
            "https://opencode.ai/zen/go/some/model",
        ] {
            assert!(is_opencode_go_endpoint(url), "should match {url}");
        }
        for url in [
            "https://opencode.ai/zen/v1",
            "https://opencode.ai/zen/gopher",
            "https://api.openai.com/v1",
            "https://opencode.evil.com/zen/go/x",
            "http://opencode.ai/zen/go/x",
        ] {
            assert!(!is_opencode_go_endpoint(url), "should not match {url}");
        }
    }
}
