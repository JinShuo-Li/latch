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
pub const USER_AGENT: &str = "latch/0.2.0";
const OPENCODE_GO_BASE: &str = "https://opencode.ai/zen/go";
const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

pub type StreamSink = Arc<dyn Fn(StreamEvent) + Send + Sync>;
/// Parses OpenAI/DeepSeek-compatible usage. Cache categories are optional: an
/// absent field stays `None` (unknown), never a fabricated zero. `input_tokens`
/// remains the provider's total prompt count; the miss count is taken from
/// `prompt_cache_miss_tokens` when present and otherwise derived from
/// total minus cache-read.
#[must_use]
pub fn openai_usage(usage: &Value) -> Option<Usage> {
    let input = usage.get("prompt_tokens").and_then(Value::as_u64)?;
    let output = usage.get("completion_tokens").and_then(Value::as_u64)?;
    let cache_read = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64));
    let cache_miss = usage
        .get("prompt_cache_miss_tokens")
        .and_then(Value::as_u64)
        .or_else(|| cache_read.map(|read| input.saturating_sub(read)));
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        cache_miss_tokens: cache_miss,
    })
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    /// Builds an equivalent transport bound to another durable session when
    /// the provider carries session-scoped wire metadata. Stateless/custom
    /// providers may return `None`, in which case the supervisor safely shares
    /// the existing implementation.
    fn for_session(&self, _session_id: Uuid) -> Option<Arc<dyn ModelProvider>> {
        None
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse>;
}

/// Wire capability for OpenAI-compatible endpoints: whether persisted
/// assistant `reasoning_content` must be replayed verbatim on the next request.
///
/// Reasoning-capable OpenAI-compatible servers (DeepSeek, and OpenCode Go
/// serving reasoning models) require every previously emitted
/// `reasoning_content` value to be passed back; generic OpenAI-compatible
/// servers reject the field. The provider derives the profile from its
/// endpoint and model, keeping the internal `ModelMessage` provider-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningReplay {
    /// Replay persisted `reasoning_content` on assistant messages.
    Replay,
    /// Never emit `reasoning_content` (providers that do not accept it).
    Omit,
}

#[must_use]
pub fn reasoning_replay_for(base_url: &str, model: &str) -> ReasoningReplay {
    let base = base_url.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();
    if is_opencode_go_endpoint(base_url)
        || base.contains("deepseek")
        || model.contains("deepseek")
        || model.contains("reasoner")
    {
        ReasoningReplay::Replay
    } else {
        ReasoningReplay::Omit
    }
}

pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    session_id: Option<Uuid>,
    reasoning: ReasoningReplay,
}
impl OpenAiProvider {
    #[must_use]
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').into(),
            api_key,
            model: model.clone(),
            session_id: None,
            reasoning: reasoning_replay_for(&base_url, &model),
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
    fn for_session(&self, session_id: Uuid) -> Option<Arc<dyn ModelProvider>> {
        Some(Arc::new(Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            session_id: Some(session_id),
            reasoning: self.reasoning,
        }))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let body = openai_request(&request, &self.model, self.reasoning);
        // The transport phase (connect + headers) obeys the same run
        // cancellation as the streaming loop, so Ctrl+C cannot hang on a
        // stalled connection.
        let response = tokio::select! {
            response = async {
                let sent = self
                    .client
                    .post(format!("{}/chat/completions", self.base_url))
                    .bearer_auth(&self.api_key)
                    .headers(self.request_headers())
                    .json(&body)
                    .send()
                    .await?;
                checked_response("openai-compatible", sent).await
            } => response?,
            () = cancel.cancelled() => bail!("model request cancelled"),
        };
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
                    usage = openai_usage(u);
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
    fn for_session(&self, _session_id: Uuid) -> Option<Arc<dyn ModelProvider>> {
        Some(Arc::new(Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
        }))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        // The transport phase obeys the same run cancellation as the stream.
        let response = tokio::select! {
            response = async {
                let sent = self
                    .client
                    .post(format!("{}/v1/messages", self.base_url))
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .headers(user_agent_headers())
                    .json(&anthropic_request(&request, &self.model))
                    .send()
                    .await?;
                checked_response("anthropic", sent).await
            } => response?,
            () = cancel.cancelled() => bail!("model request cancelled"),
        };
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut text = String::new();
        let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
        let mut stop = "end_turn".into();
        let mut usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_miss_tokens: None,
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
                            .unwrap_or(0);
                        // Anthropic reports cache read/write as explicit
                        // categories; only set them when actually present.
                        usage.cache_read_tokens = v
                            .pointer("/message/usage/cache_read_input_tokens")
                            .and_then(Value::as_u64);
                        usage.cache_write_tokens = v
                            .pointer("/message/usage/cache_creation_input_tokens")
                            .and_then(Value::as_u64);
                        // Anthropic's `input_tokens` is already the uncached
                        // portion; keep it explicit so cost never subtracts the
                        // cache-read category from it.
                        usage.cache_miss_tokens = Some(usage.input_tokens);
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
/// structurally, and reasoning is replayed verbatim for reasoning-capable
/// endpoints. Reasoning replay is decoupled from tool-call structure so a
/// defensive transform that changes tool calls can never drop required
/// reasoning state.
#[must_use]
pub fn openai_request(request: &ModelRequest, model: &str, reasoning: ReasoningReplay) -> Value {
    let mut messages = vec![json!({"role":"system","content":request.system})];
    messages.extend(
        request
            .messages
            .iter()
            .map(|message| openai_message(message, reasoning)),
    );
    json!({"model":model,"messages":messages,"tools":request.tools.iter().map(|t|json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}})).collect::<Vec<_>>(),"stream":true,"stream_options":{"include_usage":true}})
}

fn openai_message(message: &latch_protocol::ModelMessage, reasoning: ReasoningReplay) -> Value {
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
            }
            // Reasoning replay is its own wire requirement, independent of
            // tool calls: a thinking provider requires every persisted
            // reasoning_content value back, and a provider that does not
            // accept the field never produces or receives it.
            if matches!(reasoning, ReasoningReplay::Replay)
                && let Some(reasoning_content) = &message.reasoning_content
            {
                value["reasoning_content"] = Value::String(reasoning_content.clone());
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
            "user" => {
                // Anthropic requires alternating roles. A kernel context turn
                // that follows a tool result (or another user turn) is merged
                // into that user message instead of creating a second one.
                if let Some(last) = messages.last_mut()
                    && last.get("role").and_then(Value::as_str) == Some("user")
                {
                    let content = last.get_mut("content").expect("user message has content");
                    match content {
                        Value::Array(blocks) => {
                            blocks.push(json!({"type":"text","text":message.content}));
                        }
                        _ => {
                            let previous = content.take();
                            *content = json!([
                                {"type":"text","text": previous},
                                {"type":"text","text": message.content},
                            ]);
                        }
                    }
                } else {
                    messages.push(json!({"role":"user","content":message.content}));
                }
                index += 1;
            }
            role => {
                messages.push(json!({"role": role, "content": message.content}));
                index += 1;
            }
        }
    }
    // The compiled system prefix is stable per session, so the adapter marks it
    // as an ephemeral cache breakpoint. This is a provider-specific control and
    // stays at the adapter boundary.
    let system = json!([{
        "type": "text",
        "text": request.system,
        "cache_control": {"type": "ephemeral"},
    }]);
    json!({"model":model,"max_tokens":8192,"system":system,"messages":messages,"tools":request.tools.iter().map(|t|json!({"name":t.name,"description":t.description,"input_schema":t.input_schema})).collect::<Vec<_>>(),"stream":true})
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
        let o = openai_request(&r, "m", ReasoningReplay::Replay);
        assert_eq!(o["messages"][0]["role"], "system");
        assert_eq!(o["tools"][0]["function"]["name"], "read");
        let a = anthropic_request(&r, "m");
        assert_eq!(a["system"][0]["text"], "s");
        assert_eq!(a["system"][0]["cache_control"]["type"], "ephemeral");
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
        let body = openai_request(&r, "m", ReasoningReplay::Replay);
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
    fn reasoning_replay_is_decoupled_from_tool_calls() {
        // A defensive transform (sanitize_tool_history) may strip tool calls
        // from an assistant message; required reasoning must survive anyway.
        let r = ModelRequest {
            system: "s".into(),
            messages: vec![
                ModelMessage::text("user", "inspect"),
                ModelMessage {
                    role: "assistant".into(),
                    content: "working".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                    reasoning_content: Some("step by step".into()),
                },
            ],
            tools: vec![],
        };
        let replaying = openai_request(&r, "m", ReasoningReplay::Replay);
        let assistant = &replaying["messages"][2];
        assert_eq!(assistant["reasoning_content"], "step by step");
        assert!(
            assistant["tool_calls"].is_null(),
            "reasoning is emitted even when tool_calls were stripped"
        );
        // Providers that do not accept the field never receive it.
        let omitting = openai_request(&r, "m", ReasoningReplay::Omit);
        assert!(
            !omitting["messages"][2]
                .to_string()
                .contains("reasoning_content"),
            "generic OpenAI-compatible providers must not receive reasoning_content"
        );
    }

    #[test]
    fn reasoning_replay_profile_detects_deepseek_and_opencode_go() {
        for (base, model) in [
            ("https://opencode.ai/zen/go/models/gpt-5", "gpt-5"),
            ("https://api.deepseek.com", "deepseek-chat"),
            ("https://api.deepseek.com/v1", "deepseek-reasoner"),
            ("https://generic.example.com/v1", "deepseek-v4-flash"),
        ] {
            assert_eq!(
                reasoning_replay_for(base, model),
                ReasoningReplay::Replay,
                "should replay for {base} / {model}"
            );
        }
        for (base, model) in [
            ("https://api.openai.com/v1", "gpt-5-mini"),
            ("https://api.anthropic.com", "claude-sonnet"),
            ("https://generic.example.com/v1", "gpt-5"),
        ] {
            assert_eq!(
                reasoning_replay_for(base, model),
                ReasoningReplay::Omit,
                "should omit for {base} / {model}"
            );
        }
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
            "latch/0.2.0"
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

    #[test]
    fn anthropic_merges_a_kernel_context_user_turn_after_tool_results() {
        let request = ModelRequest {
            system: "stable".into(),
            tools: vec![],
            messages: vec![
                ModelMessage::text("user", "do it"),
                ModelMessage {
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: vec![latch_protocol::ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path":"a"}),
                    }],
                    tool_call_id: None,
                    reasoning_content: None,
                },
                ModelMessage {
                    role: "tool".into(),
                    content: "contents".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("c1".into()),
                    reasoning_content: None,
                },
                ModelMessage::text("user", "Kernel context: state"),
            ],
        };
        let body = anthropic_request(&request, "claude-test");
        let messages = body["messages"].as_array().unwrap();
        // user / assistant / user(tool_result + kernel context) — roles still
        // alternate, so the API accepts the request.
        assert_eq!(messages.len(), 3, "{messages:#?}");
        assert_eq!(messages[2]["role"], "user");
        let blocks = messages[2]["content"].as_array().unwrap();
        assert!(blocks.iter().any(|block| block["type"] == "tool_result"));
        assert!(blocks.iter().any(|block| {
            block["type"] == "text"
                && block["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Kernel context")
        }));
    }

    #[test]
    fn openai_usage_distinguishes_hit_miss_and_unknown() {
        // DeepSeek-style explicit hit/miss.
        let usage = openai_usage(&json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 600,
            "prompt_cache_miss_tokens": 400,
        }))
        .unwrap();
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.cache_read_tokens, Some(600));
        assert_eq!(usage.cache_miss_tokens, Some(400));
        assert_eq!(usage.uncached_input_tokens(), Some(400));

        // OpenAI-style nested cached tokens: miss is derived from the total.
        let usage = openai_usage(&json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 600},
        }))
        .unwrap();
        assert_eq!(usage.cache_read_tokens, Some(600));
        assert_eq!(usage.cache_miss_tokens, Some(400));

        // No cache accounting at all: the categories stay unknown instead of
        // being fabricated as zero.
        let usage = openai_usage(&json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
        }))
        .unwrap();
        assert_eq!(usage.cache_read_tokens, None);
        assert_eq!(usage.cache_miss_tokens, None);
        assert_eq!(usage.uncached_input_tokens(), None);

        // Missing required fields means no usage at all.
        assert!(openai_usage(&json!({"prompt_tokens": 10})).is_none());
    }
}
