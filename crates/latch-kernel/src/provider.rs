use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;
use latch_protocol::{MediaRef, ModelRequest, ModelResponse, StreamEvent, ToolCall, Usage};
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// User-Agent sent with every provider request. Keep in sync with the workspace version.
pub const USER_AGENT: &str = "latch/0.2.1";
const OPENCODE_GO_BASE: &str = "https://opencode.ai/zen/go";
const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// Resolves durable, provider-neutral media references to their immutable
/// bytes at the provider boundary. The kernel's artifact store implements
/// this; tests use an in-memory store. Bytes never enter the event log.
pub trait MediaBytesProvider: Send + Sync {
    fn read(&self, media: &MediaRef) -> Result<Vec<u8>>;
}

/// Shared handle to a media byte resolver.
pub type MediaStore = Arc<dyn MediaBytesProvider>;

/// Encodes one durable reference as a provider-facing base64 data URL. The
/// adapter resolves bytes only here, at the wire boundary, so request payloads
/// stay the only place image bytes exist.
fn media_data_url(media: &MediaRef, store: Option<&dyn MediaBytesProvider>) -> Result<String> {
    let store = store.ok_or_else(|| {
        anyhow!("image input is unavailable: no media artifact store is attached to this provider")
    })?;
    let bytes = store.read(media)?;
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
    Ok(format!("data:{};base64,{encoded}", media.mime_type))
}

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
    // DeepSeek reports chain-of-thought tokens under completion details.
    let reasoning = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64);
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        cache_miss_tokens: cache_miss,
        reasoning_tokens: reasoning,
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

/// Conservative fallback for direct construction of [`OpenAiProvider`] with
/// no resolved model capability. The registry always overrides this via
/// `with_reasoning`, so catalog/user metadata wins. A base URL never implies
/// a capability: only the model name and the OpenCode Go gateway (whose
/// unknown models may be DeepSeek-family) are consulted.
#[must_use]
pub fn reasoning_replay_for(_base_url: &str, model: &str) -> ReasoningReplay {
    let model = model.to_ascii_lowercase();
    if is_opencode_go_endpoint(_base_url)
        || model.contains("deepseek")
        || model.contains("reasoner")
    {
        ReasoningReplay::Replay
    } else {
        ReasoningReplay::Omit
    }
}

/// DeepSeek-family thinking toggle sent as `thinking: {type: enabled|disabled}`.
/// `Default` omits the parameter entirely and lets the provider decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingToggle {
    #[default]
    Default,
    Enabled,
    Disabled,
}

impl ThinkingToggle {
    #[must_use]
    pub const fn wire(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Enabled => Some("enabled"),
            Self::Disabled => Some("disabled"),
        }
    }
}

pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    provider_id: String,
    session_id: Option<Uuid>,
    reasoning: ReasoningReplay,
    effort: latch_protocol::ReasoningEffort,
    supports_effort: bool,
    thinking: ThinkingToggle,
    media: Option<MediaStore>,
}
impl OpenAiProvider {
    #[must_use]
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').into(),
            api_key,
            model: model.clone(),
            provider_id: "openai".into(),
            session_id: None,
            reasoning: reasoning_replay_for(&base_url, &model),
            effort: latch_protocol::ReasoningEffort::ProviderDefault,
            supports_effort: false,
            thinking: ThinkingToggle::Default,
            media: None,
        }
    }
    /// Selects the configured provider identity reported in durable
    /// provenance. The wire behavior stays provider-family-specific.
    #[must_use]
    pub fn with_identity(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = provider_id.into();
        self
    }
    /// Applies the resolved capability: the effort the model supports and
    /// whether persisted reasoning must be replayed. Unsupported effort values
    /// are never emitted.
    #[must_use]
    pub fn with_reasoning(
        mut self,
        effort: latch_protocol::ReasoningEffort,
        replay: ReasoningReplay,
        supports_effort: bool,
    ) -> Self {
        self.effort = effort;
        self.reasoning = replay;
        self.supports_effort = supports_effort;
        self
    }
    /// Sets the DeepSeek-family thinking toggle. Other families ignore it.
    #[must_use]
    pub fn with_thinking(mut self, thinking: ThinkingToggle) -> Self {
        self.thinking = thinking;
        self
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
    /// Attaches the resolver that turns durable media references into the
    /// inline bytes this transport requires.
    #[must_use]
    pub fn with_media(mut self, media: Option<MediaStore>) -> Self {
        self.media = media;
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
        &self.provider_id
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
            provider_id: self.provider_id.clone(),
            session_id: Some(session_id),
            reasoning: self.reasoning,
            effort: self.effort,
            supports_effort: self.supports_effort,
            thinking: self.thinking,
            media: self.media.clone(),
        }))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let effort = self.supports_effort.then(|| self.effort.wire()).flatten();
        let body = openai_request_full(
            &request,
            &self.model,
            self.reasoning,
            effort,
            self.thinking.wire(),
            self.media.as_deref(),
        )?;
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
                checked_response_redacted("openai-compatible", sent, &self.api_key).await
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
            reasoning: Vec::new(),
        };
        sink(StreamEvent::Completed(result.clone()));
        Ok(result)
    }
}

/// OpenAI Responses API adapter. Current OpenAI reasoning models require this
/// transport for tool calling with reasoning effort; the same adapter serves
/// OpenAI-compatible gateways that expose a `/responses` endpoint (for example
/// OpenCode Go's GPT models). Conversation state is local: Latch replays its
/// durable history and requests `reasoning.encrypted_content` with
/// `store: false`, so resume never depends on server-side state.
pub struct OpenAiResponsesProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    provider_id: String,
    session_id: Option<Uuid>,
    effort: latch_protocol::ReasoningEffort,
    supports_effort: bool,
    media: Option<MediaStore>,
}
impl OpenAiResponsesProvider {
    #[must_use]
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').into(),
            api_key,
            model,
            provider_id: "openai".into(),
            session_id: None,
            effort: latch_protocol::ReasoningEffort::ProviderDefault,
            supports_effort: false,
            media: None,
        }
    }
    #[must_use]
    pub fn with_identity(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = provider_id.into();
        self
    }
    #[must_use]
    pub fn with_reasoning(
        mut self,
        effort: latch_protocol::ReasoningEffort,
        supports_effort: bool,
    ) -> Self {
        self.effort = effort;
        self.supports_effort = supports_effort;
        self
    }
    #[must_use]
    pub fn with_session(mut self, session_id: Uuid) -> Self {
        self.session_id = Some(session_id);
        self
    }
    /// Attaches the resolver that turns durable media references into inline
    /// `input_image` data URLs for this transport.
    #[must_use]
    pub fn with_media(mut self, media: Option<MediaStore>) -> Self {
        self.media = media;
        self
    }
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
impl ModelProvider for OpenAiResponsesProvider {
    fn name(&self) -> &str {
        &self.provider_id
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
            provider_id: self.provider_id.clone(),
            session_id: Some(session_id),
            effort: self.effort,
            supports_effort: self.supports_effort,
            media: self.media.clone(),
        }))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let effort = self.supports_effort.then(|| self.effort.wire()).flatten();
        let body = responses_request(&request, &self.model, effort, self.media.as_deref())?;
        let response = tokio::select! {
            response = async {
                let sent = self
                    .client
                    .post(format!("{}/responses", self.base_url))
                    .bearer_auth(&self.api_key)
                    .headers(self.request_headers())
                    .json(&body)
                    .send()
                    .await?;
                checked_response_redacted("openai-responses", sent, &self.api_key).await
            } => response?,
            () = cancel.cancelled() => bail!("model request cancelled"),
        };
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut state = ResponsesStreamState::default();
        loop {
            let next = tokio::select! {()=cancel.cancelled()=>bail!("model request cancelled"),v=bytes.next()=>v};
            let Some(chunk) = next else { break };
            for data in decoder.push(&chunk?) {
                if data == "[DONE]" {
                    continue;
                }
                let v: Value = serde_json::from_str(&data)?;
                let events = state.apply(&v).map_err(|error| {
                    anyhow!("{}", crate::credentials::redact(&error, &[&self.api_key]))
                })?;
                for event in events {
                    if let StreamEvent::TextDelta(delta) = event {
                        sink(StreamEvent::TextDelta(delta));
                    }
                }
            }
        }
        let result = state.finish();
        sink(StreamEvent::Completed(result.clone()));
        Ok(result)
    }
}

/// Incremental Responses stream state. Kept separate from the transport so
/// event mapping is unit-testable without network access. Encrypted reasoning
/// items are captured wherever the stream reports them (`output_item.added`,
/// `output_item.done`, or the terminal `response.completed.output`) and
/// deduplicated by item id so a stateless replay sends each blob exactly once.
#[derive(Default)]
struct ResponsesStreamState {
    text: String,
    summary: String,
    calls: BTreeMap<String, (String, String, String)>,
    reasoning: Vec<latch_protocol::ReasoningArtifact>,
    seen_reasoning: BTreeSet<String>,
    usage: Option<Usage>,
}

impl ResponsesStreamState {
    fn apply(&mut self, v: &Value) -> Result<Vec<StreamEvent>, String> {
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                    return Ok(vec![StreamEvent::TextDelta(delta.to_owned())]);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                    self.summary.push_str(delta);
                }
            }
            "response.function_call_arguments.delta" => {
                let key = v
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                    self.calls.entry(key).or_default().2.push_str(delta);
                }
            }
            "response.output_item.added" | "response.output_item.done" | "response.completed" => {
                let items: Vec<&Value> = if let Some(item) = v.get("item") {
                    vec![item]
                } else {
                    v.pointer("/response/output")
                        .and_then(Value::as_array)
                        .map(|items| items.iter().collect())
                        .unwrap_or_default()
                };
                for item in items {
                    match item.get("type").and_then(Value::as_str) {
                        Some("function_call") => {
                            let key = item
                                .get("id")
                                .and_then(Value::as_str)
                                .or_else(|| item.get("call_id").and_then(Value::as_str))
                                .unwrap_or_default()
                                .to_owned();
                            let entry = self.calls.entry(key).or_default();
                            if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                                entry.0 = call_id.to_owned();
                            }
                            if let Some(name) = item.get("name").and_then(Value::as_str) {
                                entry.1 = name.to_owned();
                            }
                            if let Some(arguments) = item.get("arguments").and_then(Value::as_str)
                                && !arguments.is_empty()
                            {
                                entry.2 = arguments.to_owned();
                            }
                        }
                        Some("reasoning") => {
                            if let Some(data) =
                                item.get("encrypted_content").and_then(Value::as_str)
                                && !data.is_empty()
                            {
                                let key = item.get("id").and_then(Value::as_str).map_or_else(
                                    || format!("data:{data}"),
                                    |id| format!("id:{id}"),
                                );
                                if self.seen_reasoning.insert(key) {
                                    self.reasoning.push(
                                        latch_protocol::ReasoningArtifact::Encrypted {
                                            data: data.to_owned(),
                                        },
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if v.get("type").and_then(Value::as_str) == Some("response.completed") {
                    self.usage = v.pointer("/response/usage").and_then(responses_usage);
                }
            }
            "response.failed" | "response.incomplete" => {
                let detail = v
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("response failed");
                return Err(format!("OpenAI Responses error: {detail}"));
            }
            "error" => {
                let detail = v
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("stream error");
                return Err(format!("OpenAI Responses stream error: {detail}"));
            }
            _ => {}
        }
        Ok(Vec::new())
    }

    fn finish(self) -> ModelResponse {
        let tool_calls = self
            .calls
            .into_values()
            .filter(|(_, name, _)| !name.is_empty())
            .filter_map(|(id, name, args)| {
                let arguments = if args.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&args).ok()?
                };
                Some(ToolCall {
                    id,
                    name,
                    arguments,
                })
            })
            .collect();
        ModelResponse {
            text: self.text,
            tool_calls,
            stop_reason: "completed".to_owned(),
            usage: self.usage,
            reasoning_content: (!self.summary.is_empty()).then_some(self.summary),
            reasoning: self.reasoning,
        }
    }
}

/// Serializes a provider-neutral request into the OpenAI Responses API shape.
/// The request is stateless (`store: false`) and asks for encrypted reasoning
/// so the durable Latch history alone is enough to continue a tool loop.
///
/// User turns with images become the documented `input_image` content parts
/// carrying base64 data URLs, and tool results with images use the documented
/// `function_call_output` array form. Text-only traffic serializes exactly as
/// before.
pub fn responses_request(
    request: &ModelRequest,
    model: &str,
    effort: Option<&str>,
    media: Option<&dyn MediaBytesProvider>,
) -> Result<Value> {
    let mut input: Vec<Value> = Vec::new();
    for message in &request.messages {
        match message.role.as_str() {
            "assistant" => {
                for artifact in &message.reasoning {
                    if let latch_protocol::ReasoningArtifact::Encrypted { data } = artifact
                        && !data.is_empty()
                    {
                        input.push(json!({
                            "type": "reasoning",
                            "encrypted_content": data,
                            "summary": [],
                        }));
                    }
                }
                if !message.content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": message.content}],
                    }));
                }
                for call in &message.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": serde_json::to_string(&call.arguments)
                            .unwrap_or_else(|_| "{}".into()),
                    }));
                }
            }
            "tool" => {
                if message.media.is_empty() {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": message.tool_call_id.clone().unwrap_or_default(),
                        "output": message.content,
                    }));
                } else {
                    // Documented array form: text and images as input content
                    // parts, so the terminal tool transaction stays valid.
                    let mut parts = Vec::new();
                    if !message.content.is_empty() {
                        parts.push(json!({"type": "input_text", "text": message.content}));
                    }
                    for media_ref in &message.media {
                        parts.push(json!({
                            "type": "input_image",
                            "image_url": media_data_url(media_ref, media)?,
                        }));
                    }
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": message.tool_call_id.clone().unwrap_or_default(),
                        "output": parts,
                    }));
                }
            }
            role => {
                if message.media.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": role,
                        "content": [{"type": "input_text", "text": message.content}],
                    }));
                } else {
                    let mut content = Vec::new();
                    for media_ref in &message.media {
                        content.push(json!({
                            "type": "input_image",
                            "image_url": media_data_url(media_ref, media)?,
                        }));
                    }
                    if !message.content.is_empty() {
                        content.push(json!({"type": "input_text", "text": message.content}));
                    }
                    input.push(json!({
                        "type": "message",
                        "role": role,
                        "content": content,
                    }));
                }
            }
        }
    }
    let mut body = json!({
        "model": model,
        "instructions": request.system,
        "input": input,
        "tools": request
            .tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            }))
            .collect::<Vec<_>>(),
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort": effort});
    }
    Ok(body)
}

/// Maps Responses usage. Cache and reasoning categories stay `None` when the
/// provider did not report them.
#[must_use]
pub fn responses_usage(usage: &Value) -> Option<Usage> {
    let input = usage.get("input_tokens").and_then(Value::as_u64)?;
    let output = usage.get("output_tokens").and_then(Value::as_u64)?;
    let cache_read = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64);
    let cache_miss = cache_read.map(|read| input.saturating_sub(read));
    let reasoning = usage
        .pointer("/output_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64);
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        cache_miss_tokens: cache_miss,
        reasoning_tokens: reasoning,
    })
}

pub struct AnthropicProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    provider_id: String,
    session_id: Option<Uuid>,
    effort: latch_protocol::ReasoningEffort,
    supports_effort: bool,
    adaptive_thinking: bool,
    media: Option<MediaStore>,
}
impl AnthropicProvider {
    #[must_use]
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').into(),
            api_key,
            model,
            provider_id: "anthropic".into(),
            session_id: None,
            effort: latch_protocol::ReasoningEffort::ProviderDefault,
            supports_effort: false,
            adaptive_thinking: false,
            media: None,
        }
    }
    /// Selects the configured provider identity reported in durable provenance.
    #[must_use]
    pub fn with_identity(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = provider_id.into();
        self
    }
    /// Applies resolved capabilities: the selectable effort and whether the
    /// model supports adaptive thinking.
    #[must_use]
    pub fn with_reasoning(
        mut self,
        effort: latch_protocol::ReasoningEffort,
        supports_effort: bool,
        adaptive_thinking: bool,
    ) -> Self {
        self.effort = effort;
        self.supports_effort = supports_effort;
        self.adaptive_thinking = adaptive_thinking;
        self
    }
    /// OpenCode Go also exposes Anthropic-Messages models; they receive the
    /// same stable session header as the OpenAI-compatible transports.
    #[must_use]
    pub fn with_session(mut self, session_id: Uuid) -> Self {
        self.session_id = Some(session_id);
        self
    }
    /// Attaches the resolver that turns durable media references into base64
    /// image blocks for the Messages transport.
    #[must_use]
    pub fn with_media(mut self, media: Option<MediaStore>) -> Self {
        self.media = media;
        self
    }
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
impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.provider_id
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
            provider_id: self.provider_id.clone(),
            session_id: Some(session_id),
            effort: self.effort,
            supports_effort: self.supports_effort,
            adaptive_thinking: self.adaptive_thinking,
            media: self.media.clone(),
        }))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let effort = self.supports_effort.then(|| self.effort.wire()).flatten();
        let config = AnthropicConfig {
            adaptive_thinking: self.adaptive_thinking,
            effort,
        };
        // The transport phase obeys the same run cancellation as the stream.
        let response = tokio::select! {
            response = async {
                let sent = self
                    .client
                    .post(format!("{}/v1/messages", self.base_url))
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .headers(self.request_headers())
                    .json(&anthropic_request_with_config(
                        &request,
                        &self.model,
                        config,
                        self.media.as_deref(),
                    )?)
                    .send()
                    .await?;
                checked_response_redacted("anthropic", sent, &self.api_key).await
            } => response?,
            () = cancel.cancelled() => bail!("model request cancelled"),
        };
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut text = String::new();
        let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
        // Thinking blocks are captured structurally: readable text plus the
        // opaque signature (or `data` for redacted blocks). They are replayed
        // unchanged on the next tool turn and never rendered as assistant text.
        let mut thinking: BTreeMap<u64, ThinkingBlock> = BTreeMap::new();
        let mut stop = "end_turn".into();
        let mut usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_miss_tokens: None,
            reasoning_tokens: None,
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
                        let index = v["index"].as_u64().unwrap_or(0);
                        match v.pointer("/content_block/type").and_then(Value::as_str) {
                            Some("tool_use") => {
                                calls.insert(
                                    index,
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
                            Some("thinking") => {
                                thinking.insert(
                                    index,
                                    ThinkingBlock {
                                        text: v
                                            .pointer("/content_block/thinking")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_owned(),
                                        signature: v
                                            .pointer("/content_block/signature")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_owned(),
                                        redacted: None,
                                    },
                                );
                            }
                            Some("redacted_thinking") => {
                                thinking.insert(
                                    index,
                                    ThinkingBlock {
                                        text: String::new(),
                                        signature: String::new(),
                                        redacted: Some(
                                            v.pointer("/content_block/data")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_owned(),
                                        ),
                                    },
                                );
                            }
                            _ => {}
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
                            Some("thinking_delta") => {
                                if let Some(block) =
                                    thinking.get_mut(&v["index"].as_u64().unwrap_or(0))
                                {
                                    block.text.push_str(
                                        v.pointer("/delta/thinking")
                                            .and_then(Value::as_str)
                                            .unwrap_or(""),
                                    );
                                }
                            }
                            Some("signature_delta") => {
                                if let Some(block) =
                                    thinking.get_mut(&v["index"].as_u64().unwrap_or(0))
                                {
                                    block.signature.push_str(
                                        v.pointer("/delta/signature")
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
        let reasoning = thinking
            .into_values()
            .filter_map(|block| match block.redacted {
                Some(data) if !data.is_empty() => {
                    Some(latch_protocol::ReasoningArtifact::Redacted { data })
                }
                Some(_) => None,
                None => {
                    // A thinking block with neither text nor signature carries
                    // no replayable material.
                    (!block.signature.is_empty() || !block.text.is_empty()).then_some(
                        latch_protocol::ReasoningArtifact::Thinking {
                            text: block.text,
                            signature: block.signature,
                        },
                    )
                }
            })
            .collect();
        let result = ModelResponse {
            text,
            tool_calls,
            stop_reason: stop,
            usage: Some(usage),
            reasoning_content: None,
            reasoning,
        };
        sink(StreamEvent::Completed(result.clone()));
        Ok(result)
    }
}

/// One in-flight Anthropic thinking block.
#[derive(Default)]
struct ThinkingBlock {
    text: String,
    signature: String,
    redacted: Option<String>,
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
/// credentials) are never included. Any occurrence of the API key in the
/// provider's error body is redacted before the error can reach a transcript
/// or a log.
async fn checked_response_redacted(
    provider: &str,
    response: reqwest::Response,
    secret: &str,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let body = if secret.is_empty() {
        body
    } else {
        crate::credentials::redact(&body, &[secret])
    };
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
///
/// Chat Completions tool messages do not carry images, so tool media is
/// preserved by the documented fallback: every terminal textual tool result is
/// emitted first, then one adjacent user observation turn with the images. No
/// user turn is ever inserted between a tool call and its terminal result.
pub fn openai_request(
    request: &ModelRequest,
    model: &str,
    reasoning: ReasoningReplay,
) -> Result<Value> {
    openai_request_full(request, model, reasoning, None, None, None)
}

/// Like [`openai_request`], but emits `reasoning_effort` only when the resolved
/// model capability selected a concrete effort value. A `None` effort never
/// adds the field, so unsupported DeepSeek/OpenAI parameters can never leak to
/// unrelated OpenAI-compatible endpoints.
pub fn openai_request_with_effort(
    request: &ModelRequest,
    model: &str,
    reasoning: ReasoningReplay,
    effort: Option<&str>,
) -> Result<Value> {
    openai_request_full(request, model, reasoning, effort, None, None)
}

/// Full chat-completions serialization. `thinking` is the DeepSeek-family
/// toggle (`thinking: {type: enabled|disabled}`) and is never sent when the
/// resolved capability does not provide one. `media` resolves durable image
/// references when any are present.
pub fn openai_request_full(
    request: &ModelRequest,
    model: &str,
    reasoning: ReasoningReplay,
    effort: Option<&str>,
    thinking: Option<&str>,
    media: Option<&dyn MediaBytesProvider>,
) -> Result<Value> {
    /// Deterministic adjacent-turn note for images that a tool produced.
    const TOOL_IMAGE_NOTE: &str = "Images returned by the tool call(s) above.";
    let mut messages = vec![json!({"role":"system","content":request.system})];
    // Images from consecutive terminal tool results are held until the tool
    // run ends, so they can never split a tool call from its result.
    let mut pending_tool_media: Vec<&MediaRef> = Vec::new();
    let flush = |messages: &mut Vec<Value>, pending: &mut Vec<&MediaRef>| -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let mut content = Vec::new();
        for media_ref in pending.iter() {
            content.push(json!({
                "type": "image_url",
                "image_url": {"url": media_data_url(media_ref, media)?},
            }));
        }
        content.push(json!({"type": "text", "text": TOOL_IMAGE_NOTE}));
        messages.push(json!({"role": "user", "content": content}));
        pending.clear();
        Ok(())
    };
    for message in &request.messages {
        if message.role != "tool" {
            flush(&mut messages, &mut pending_tool_media)?;
        }
        if message.role == "tool" && !message.media.is_empty() {
            messages.push(openai_message(message, reasoning));
            pending_tool_media.extend(message.media.iter());
            continue;
        }
        messages.push(openai_message_media_aware(message, reasoning, media)?);
    }
    flush(&mut messages, &mut pending_tool_media)?;
    let mut body = json!({"model":model,"messages":messages,"tools":request.tools.iter().map(|t|json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}})).collect::<Vec<_>>(),"stream":true,"stream_options":{"include_usage":true}});
    if let Some(effort) = effort {
        body["reasoning_effort"] = Value::String(effort.to_owned());
    }
    if let Some(thinking) = thinking {
        body["thinking"] = json!({"type": thinking});
    }
    Ok(body)
}

/// Serializes one non-tool message, upgrading to multimodal content parts when
/// the message carries images.
fn openai_message_media_aware(
    message: &latch_protocol::ModelMessage,
    reasoning: ReasoningReplay,
    media: Option<&dyn MediaBytesProvider>,
) -> Result<Value> {
    if message.media.is_empty() {
        return Ok(openai_message(message, reasoning));
    }
    if message.role == "user" || message.role == "developer" {
        let mut content = Vec::new();
        for media_ref in &message.media {
            content.push(json!({
                "type": "image_url",
                "image_url": {"url": media_data_url(media_ref, media)?},
            }));
        }
        if !message.content.is_empty() {
            content.push(json!({"type": "text", "text": message.content}));
        }
        return Ok(json!({"role": message.role, "content": content}));
    }
    // Assistant-side images are never produced by Latch; text-only is the
    // honest serialization rather than inventing a role that no API accepts.
    Ok(openai_message(message, reasoning))
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

/// Resolved Anthropic request controls.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicConfig {
    /// Enable adaptive thinking with summarized display.
    pub adaptive_thinking: bool,
    /// Explicit `output_config.effort`; `None` lets the API use its default.
    pub effort: Option<&'static str>,
}

/// Serializes a provider-independent request into the Anthropic Messages wire
/// format. Tool calls become `tool_use` blocks and tool results become
/// `tool_result` blocks. Thinking and redacted-thinking artifacts are echoed
/// back unchanged and in their original order ahead of text/tool blocks, which
/// is required for tool-use continuation; OpenAI-only fields such as
/// `reasoning_content` are never emitted.
///
/// Images serialize as official base64 `image` blocks: ahead of the text in a
/// user turn, and nested inside `tool_result.content` for tool output.
pub fn anthropic_request_with_config(
    request: &ModelRequest,
    model: &str,
    config: AnthropicConfig,
    media: Option<&dyn MediaBytesProvider>,
) -> Result<Value> {
    let image_block = |media_ref: &MediaRef| -> Result<Value> {
        let store = media.ok_or_else(|| {
            anyhow!(
                "image input is unavailable: no media artifact store is attached to this provider"
            )
        })?;
        let bytes = store.read(media_ref)?;
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
        Ok(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_ref.mime_type,
                "data": encoded,
            },
        }))
    };
    let mut messages: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < request.messages.len() {
        let message = &request.messages[index];
        match message.role.as_str() {
            "assistant" => {
                let mut blocks = Vec::new();
                // Thinking blocks must precede other content in the assistant
                // turn and must be byte-identical to what the model returned.
                for artifact in &message.reasoning {
                    match artifact {
                        latch_protocol::ReasoningArtifact::Thinking { text, signature }
                            if !signature.is_empty() =>
                        {
                            blocks.push(json!({
                                "type": "thinking",
                                "thinking": text,
                                "signature": signature,
                            }));
                        }
                        latch_protocol::ReasoningArtifact::Redacted { data }
                            if !data.is_empty() =>
                        {
                            blocks.push(json!({
                                "type": "redacted_thinking",
                                "data": data,
                            }));
                        }
                        // Text/encrypted artifacts belong to other transports.
                        _ => {}
                    }
                }
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
                    let tool = &request.messages[index];
                    let content = if tool.media.is_empty() {
                        Value::String(tool.content.clone())
                    } else {
                        let mut parts = Vec::new();
                        if !tool.content.is_empty() {
                            parts.push(json!({"type":"text","text": tool.content}));
                        }
                        for media_ref in &tool.media {
                            parts.push(image_block(media_ref)?);
                        }
                        Value::Array(parts)
                    };
                    blocks.push(json!({
                        "type":"tool_result",
                        "tool_use_id": tool.tool_call_id.clone().unwrap_or_default(),
                        "content": content,
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
                    // Images precede text in the merged user turn, matching the
                    // single-message ordering.
                    let mut extra = Vec::new();
                    for media_ref in &message.media {
                        extra.push(image_block(media_ref)?);
                    }
                    if !message.content.is_empty() {
                        extra.push(json!({"type":"text","text":message.content}));
                    }
                    let content = last.get_mut("content").expect("user message has content");
                    match content {
                        Value::Array(blocks) => blocks.extend(extra),
                        _ => {
                            let previous = content.take();
                            let mut merged = vec![json!({"type":"text","text": previous})];
                            merged.extend(extra);
                            *content = Value::Array(merged);
                        }
                    }
                } else if message.media.is_empty() {
                    messages.push(json!({"role":"user","content":message.content}));
                } else {
                    let mut blocks = Vec::new();
                    for media_ref in &message.media {
                        blocks.push(image_block(media_ref)?);
                    }
                    if !message.content.is_empty() {
                        blocks.push(json!({"type":"text","text":message.content}));
                    }
                    messages.push(json!({"role":"user","content":blocks}));
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
    // Adaptive thinking plus a large max_tokens: thinking counts against
    // max_tokens on Anthropic models, so a summary-sized budget would truncate.
    let max_tokens = if config.adaptive_thinking {
        32_000
    } else {
        8_192
    };
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": messages,
        "tools": request.tools.iter().map(|t| json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.input_schema,
        })).collect::<Vec<_>>(),
        "stream": true,
    });
    if config.adaptive_thinking {
        body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
        if let Some(effort) = config.effort {
            body["output_config"] = json!({"effort": effort});
        }
    }
    Ok(body)
}

/// Compatibility wrapper for callers that do not configure thinking.
pub fn anthropic_request(request: &ModelRequest, model: &str) -> Result<Value> {
    anthropic_request_with_config(request, model, AnthropicConfig::default(), None)
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
        let o = openai_request(&r, "m", ReasoningReplay::Replay).unwrap();
        assert_eq!(o["messages"][0]["role"], "system");
        assert_eq!(o["tools"][0]["function"]["name"], "read");
        let a = anthropic_request(&r, "m").unwrap();
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

                    reasoning: vec![],
                    media: Vec::new(),
                },
                ModelMessage {
                    role: "tool".into(),
                    content: "contents".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("call-1".into()),
                    reasoning_content: None,

                    reasoning: vec![],
                    media: Vec::new(),
                },
            ],
            tools: vec![],
        };
        let body = openai_request(&r, "m", ReasoningReplay::Replay).unwrap();
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

                    reasoning: vec![],
                    media: Vec::new(),
                },
            ],
            tools: vec![],
        };
        let replaying = openai_request(&r, "m", ReasoningReplay::Replay).unwrap();
        let assistant = &replaying["messages"][2];
        assert_eq!(assistant["reasoning_content"], "step by step");
        assert!(
            assistant["tool_calls"].is_null(),
            "reasoning is emitted even when tool_calls were stripped"
        );
        // Providers that do not accept the field never receive it.
        let omitting = openai_request(&r, "m", ReasoningReplay::Omit).unwrap();
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

                    reasoning: vec![],
                    media: Vec::new(),
                },
                ModelMessage {
                    role: "tool".into(),
                    content: "contents".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("call-1".into()),
                    reasoning_content: None,

                    reasoning: vec![],
                    media: Vec::new(),
                },
            ],
            tools: vec![],
        };
        let body = anthropic_request(&r, "m").unwrap();
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
    fn reasoning_effort_is_only_emitted_when_selected() {
        let request = ModelRequest {
            system: "s".into(),
            messages: vec![ModelMessage::text("user", "hi")],
            tools: vec![],
        };
        for effort in ["low", "high", "max"] {
            let body = openai_request_with_effort(
                &request,
                "deepseek-v4.1-flash",
                ReasoningReplay::Replay,
                Some(effort),
            )
            .unwrap();
            assert_eq!(
                body["reasoning_effort"], effort,
                "DeepSeek V4 emits the documented value"
            );
        }
        // No selected effort: the field must not exist at all.
        let body = openai_request_with_effort(
            &request,
            "deepseek-v4.1-flash",
            ReasoningReplay::Replay,
            None,
        )
        .unwrap();
        assert!(body.get("reasoning_effort").is_none());
        // Generic OpenAI-compatible endpoints never receive the parameter.
        let body =
            openai_request_with_effort(&request, "custom-model", ReasoningReplay::Omit, None)
                .unwrap();
        assert!(body.get("reasoning_effort").is_none());
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
            "latch/0.2.1"
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

                    reasoning: vec![],
                    media: Vec::new(),
                },
                ModelMessage {
                    role: "tool".into(),
                    content: "contents".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("c1".into()),
                    reasoning_content: None,

                    reasoning: vec![],
                    media: Vec::new(),
                },
                ModelMessage::text("user", "Kernel context: state"),
            ],
        };
        let body = anthropic_request(&request, "claude-test").unwrap();
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

    #[test]
    fn responses_request_maps_a_stateless_tool_transaction() {
        let request = ModelRequest {
            system: "stable".into(),
            messages: vec![
                ModelMessage::text("user", "fix it"),
                ModelMessage {
                    role: "assistant".into(),
                    content: "working".into(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "a.txt"}),
                    }],
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: vec![latch_protocol::ReasoningArtifact::Encrypted {
                        data: "enc-blob".into(),
                    }],
                    media: Vec::new(),
                },
                ModelMessage {
                    role: "tool".into(),
                    content: "contents".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("call-1".into()),
                    reasoning_content: None,
                    reasoning: Vec::new(),
                    media: Vec::new(),
                },
            ],
            tools: vec![ToolDefinition {
                name: "read_file".into(),
                description: "read".into(),
                input_schema: json!({"type": "object"}),
            }],
        };
        let body = responses_request(&request, "gpt-6-astra", Some("high"), None).unwrap();
        assert_eq!(body["model"], "gpt-6-astra");
        assert_eq!(body["instructions"], "stable");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["encrypted_content"], "enc-blob");
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "call-1");
        assert_eq!(input[3]["arguments"], "{\"path\":\"a.txt\"}");
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "call-1");
    }

    #[test]
    fn responses_request_omits_reasoning_for_provider_default() {
        let request = ModelRequest {
            system: "s".into(),
            messages: vec![ModelMessage::text("user", "hi")],
            tools: vec![],
        };
        let body = responses_request(&request, "m", None, None).unwrap();
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn responses_stream_assembles_text_tool_calls_and_usage() {
        let mut state = ResponsesStreamState::default();
        let text = state
            .apply(&json!({"type":"response.output_text.delta","delta":"hello "}))
            .unwrap();
        assert_eq!(text, vec![StreamEvent::TextDelta("hello ".into())]);
        state
            .apply(&json!({"type":"response.output_text.delta","delta":"world"}))
            .unwrap();
        state
            .apply(&json!({"type":"response.reasoning_summary_text.delta","delta":"plan"}))
            .unwrap();
        // Arguments stream in fragments, then the item is finalized.
        state
            .apply(&json!({
                "type":"response.function_call_arguments.delta",
                "item_id":"fc_1",
                "delta":"{\"path\":"
            }))
            .unwrap();
        state
            .apply(&json!({
                "type":"response.function_call_arguments.delta",
                "item_id":"fc_1",
                "delta":"\"a.txt\"}"
            }))
            .unwrap();
        state
            .apply(&json!({
                "type":"response.output_item.done",
                "item":{"id":"fc_1","type":"function_call","call_id":"call-9","name":"read_file"}
            }))
            .unwrap();
        state
            .apply(&json!({
                "type":"response.completed",
                "response":{
                    "output":[],
                    "usage":{
                        "input_tokens":100,
                        "output_tokens":40,
                        "input_tokens_details":{"cached_tokens":80},
                        "output_tokens_details":{"reasoning_tokens":25}
                    }
                }
            }))
            .unwrap();
        let response = state.finish();
        assert_eq!(response.text, "hello world");
        assert_eq!(response.reasoning_content.as_deref(), Some("plan"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call-9");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.tool_calls[0].arguments["path"], "a.txt");
        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cache_read_tokens, Some(80));
        assert_eq!(usage.cache_miss_tokens, Some(20));
        assert_eq!(usage.reasoning_tokens, Some(25));
    }

    #[test]
    fn responses_stream_captures_encrypted_reasoning_once() {
        let mut state = ResponsesStreamState::default();
        // Visible summary and opaque encrypted content are separate: only the
        // summary may ever surface as reasoning text.
        state
            .apply(&json!({"type":"response.reasoning_summary_text.delta","delta":"plan"}))
            .unwrap();
        // The same reasoning item may be reported by several event shapes.
        state
            .apply(&json!({
                "type":"response.output_item.added",
                "item":{"id":"rs_1","type":"reasoning","encrypted_content":"opaque-ciphertext"}
            }))
            .unwrap();
        state
            .apply(&json!({
                "type":"response.output_item.done",
                "item":{"id":"rs_1","type":"reasoning","encrypted_content":"opaque-ciphertext"}
            }))
            .unwrap();
        state
            .apply(&json!({
                "type":"response.completed",
                "response":{
                    "output":[{"id":"rs_1","type":"reasoning","encrypted_content":"opaque-ciphertext"}],
                    "usage":null
                }
            }))
            .unwrap();
        let response = state.finish();
        assert_eq!(response.reasoning_content.as_deref(), Some("plan"));
        assert_eq!(
            response.reasoning,
            vec![latch_protocol::ReasoningArtifact::Encrypted {
                data: "opaque-ciphertext".into(),
            }]
        );
    }

    #[test]
    fn responses_stateless_tool_loop_replays_parser_encrypted_reasoning_once() {
        // Request 1: decode the Responses events exactly as the streaming
        // transport does. The artifact must come from the parser, never from a
        // hand-built `ModelResponse`.
        let mut state = ResponsesStreamState::default();
        state
            .apply(&json!({
                "type":"response.output_item.done",
                "item":{"id":"rs_1","type":"reasoning","encrypted_content":"opaque-ciphertext"}
            }))
            .unwrap();
        state
            .apply(&json!({
                "type":"response.function_call_arguments.delta",
                "item_id":"fc_1",
                "delta":"{\"path\":\"calc.py\"}"
            }))
            .unwrap();
        state
            .apply(&json!({
                "type":"response.output_item.done",
                "item":{"id":"fc_1","type":"function_call","call_id":"call-1","name":"read_file"}
            }))
            .unwrap();
        let first = state.finish();
        let encrypted = latch_protocol::ReasoningArtifact::Encrypted {
            data: "opaque-ciphertext".into(),
        };
        assert_eq!(first.reasoning, vec![encrypted.clone()]);

        // Persist through the real durable store exactly as the agent does,
        // then reopen the file to simulate a process/session resume.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.sqlite3");
        let session;
        {
            let store = crate::store::EventStore::open(&db).unwrap();
            session = store.create_session(dir.path()).unwrap();
            store
                .append(
                    session,
                    latch_protocol::EventPayload::UserMessage {
                        text: "Fix the bug in calc.py and verify it.".into(),
                        media: vec![],
                    },
                )
                .unwrap();
            store
                .append(
                    session,
                    latch_protocol::EventPayload::AssistantMessageCompleted {
                        text: first.text.clone(),
                        tool_calls: first.tool_calls.clone(),
                        reasoning_content: first.reasoning_content.clone(),
                        reasoning: first.reasoning.clone(),
                    },
                )
                .unwrap();
            store
                .append(
                    session,
                    latch_protocol::EventPayload::ToolCompleted {
                        result: latch_protocol::ToolResult {
                            call_id: "call-1".into(),
                            name: "read_file".into(),
                            output: "def add(a, b):\n    return a - b".into(),
                            is_error: false,
                            artifact_id: None,
                            media: Vec::new(),
                        },
                    },
                )
                .unwrap();
        }
        let resumed = crate::store::EventStore::open(&db).unwrap();
        let ctx = crate::continuity::MaterializedContext {
            system: String::new(),
            session_context: String::new(),
            canonical: String::new(),
            recalled: String::new(),
            recent: resumed.events(session).unwrap(),
            bridge: crate::continuity::ConversationBridge::default(),
            episodes: vec![],
            stats: latch_protocol::ContextStats::default(),
        };
        let request = ModelRequest {
            system: "stable".into(),
            messages: crate::agent::request::context_messages(&ctx),
            tools: vec![],
        };
        // Reconstruction is exact: the durable assistant turn rebuilds the
        // parser-produced artifact without modification.
        let assistant = request
            .messages
            .iter()
            .find(|message| message.role == "assistant")
            .unwrap();
        assert_eq!(assistant.reasoning, vec![encrypted]);

        // Request 2 replays exactly one encrypted item, before the function
        // call it belongs to, followed by the tool output.
        let body = responses_request(&request, "gpt-6-astra", Some("high"), None).unwrap();
        assert_eq!(body["store"], false);
        assert!(
            body["include"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("reasoning.encrypted_content"))
        );
        let input = body["input"].as_array().unwrap();
        let occurrences = input
            .iter()
            .filter(|item| item["encrypted_content"] == "opaque-ciphertext")
            .count();
        assert_eq!(
            occurrences, 1,
            "encrypted reasoning must be replayed exactly once"
        );
        let reasoning = input
            .iter()
            .position(|item| item["type"] == "reasoning")
            .unwrap();
        let call = input
            .iter()
            .position(|item| item["type"] == "function_call")
            .unwrap();
        let output = input
            .iter()
            .position(|item| item["type"] == "function_call_output")
            .unwrap();
        assert!(reasoning < call && call < output);
    }

    #[test]
    fn responses_stream_reports_provider_errors() {
        let mut state = ResponsesStreamState::default();
        let error = state
            .apply(&json!({
                "type":"response.failed",
                "response":{"error":{"message":"bad request"}}
            }))
            .unwrap_err();
        assert!(error.contains("bad request"));
    }

    #[test]
    fn anthropic_thinking_replay_preserves_blocks_order_and_effort() {
        let request = ModelRequest {
            system: "stable".into(),
            messages: vec![
                ModelMessage::text("user", "weather?"),
                ModelMessage {
                    role: "assistant".into(),
                    content: "checking".into(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "get_weather".into(),
                        arguments: json!({"city": "Paris"}),
                    }],
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: vec![
                        latch_protocol::ReasoningArtifact::Thinking {
                            text: "I should call the tool".into(),
                            signature: "sig-1".into(),
                        },
                        latch_protocol::ReasoningArtifact::Redacted {
                            data: "opaque".into(),
                        },
                    ],
                    media: Vec::new(),
                },
                ModelMessage {
                    role: "tool".into(),
                    content: "18C".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("call-1".into()),
                    reasoning_content: None,
                    reasoning: Vec::new(),
                    media: Vec::new(),
                },
            ],
            tools: vec![],
        };
        let body = anthropic_request_with_config(
            &request,
            "claude-opus-5",
            AnthropicConfig {
                adaptive_thinking: true,
                effort: Some("high"),
            },
            None,
        )
        .unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(body["output_config"]["effort"], "high");
        let assistant = &body["messages"][1];
        let blocks = assistant["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "I should call the tool");
        assert_eq!(blocks[0]["signature"], "sig-1");
        assert_eq!(blocks[1]["type"], "redacted_thinking");
        assert_eq!(blocks[1]["data"], "opaque");
        assert_eq!(blocks[2]["type"], "text");
        assert_eq!(blocks[3]["type"], "tool_use");
        assert_eq!(blocks[3]["id"], "call-1");
        // Tool results group into one user turn after the assistant turn.
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call-1");
        // Without adaptive thinking no thinking configuration is sent.
        let plain = anthropic_request_with_config(
            &request,
            "claude-haiku-4-5",
            AnthropicConfig::default(),
            None,
        )
        .unwrap();
        assert!(plain.get("thinking").is_none());
        assert!(plain.get("output_config").is_none());
    }

    #[test]
    fn openai_chat_emits_deepseek_thinking_toggle_only_when_set() {
        let request = ModelRequest {
            system: "s".into(),
            messages: vec![ModelMessage::text("user", "hi")],
            tools: vec![],
        };
        let enabled = openai_request_full(
            &request,
            "deepseek-flash",
            ReasoningReplay::Replay,
            Some("high"),
            Some("enabled"),
            None,
        )
        .unwrap();
        assert_eq!(enabled["thinking"]["type"], "enabled");
        assert_eq!(enabled["reasoning_effort"], "high");
        let disabled = openai_request_full(
            &request,
            "deepseek-flash",
            ReasoningReplay::Replay,
            None,
            Some("disabled"),
            None,
        )
        .unwrap();
        assert_eq!(disabled["thinking"]["type"], "disabled");
        assert!(disabled.get("reasoning_effort").is_none());
        let omitted = openai_request_full(
            &request,
            "deepseek-flash",
            ReasoningReplay::Replay,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(omitted.get("thinking").is_none());
    }

    /// In-memory media resolver for wire-serialization tests.
    struct MemoryMedia(std::collections::HashMap<String, Vec<u8>>);

    impl MediaBytesProvider for MemoryMedia {
        fn read(&self, media: &MediaRef) -> Result<Vec<u8>> {
            self.0
                .get(&media.id)
                .cloned()
                .ok_or_else(|| anyhow!("missing media {}", media.id))
        }
    }

    fn image_ref(id: &str) -> MediaRef {
        MediaRef {
            id: id.into(),
            kind: latch_protocol::MediaKind::Image,
            mime_type: "image/png".into(),
            artifact_path: format!("media/{id}.png"),
            sha256: id.into(),
            byte_len: 3,
            width: Some(1440),
            height: Some(900),
            display_name: Some(format!("{id}.png")),
        }
    }

    fn media_store() -> MemoryMedia {
        MemoryMedia(
            [
                ("img-a".to_owned(), vec![1, 2, 3]),
                ("img-b".to_owned(), vec![4, 5]),
            ]
            .into_iter()
            .collect(),
        )
    }

    fn multimodal_request() -> ModelRequest {
        let user = ModelMessage {
            role: "user".into(),
            content: "inspect".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning: vec![],
            media: vec![image_ref("img-a"), image_ref("img-b")],
        };
        let assistant = ModelMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![
                ToolCall {
                    id: "call-media".into(),
                    name: "read_image".into(),
                    arguments: json!({"path": "shot.png"}),
                },
                ToolCall {
                    id: "call-text".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a.txt"}),
                },
            ],
            tool_call_id: None,
            reasoning_content: None,
            reasoning: vec![],
            media: vec![],
        };
        let tool_media = ModelMessage {
            role: "tool".into(),
            content: "image: [image: shot.png · 1440×900]".into(),
            tool_calls: vec![],
            tool_call_id: Some("call-media".into()),
            reasoning_content: None,
            reasoning: vec![],
            media: vec![image_ref("img-b")],
        };
        let tool_text = ModelMessage {
            role: "tool".into(),
            content: "contents".into(),
            tool_calls: vec![],
            tool_call_id: Some("call-text".into()),
            reasoning_content: None,
            reasoning: vec![],
            media: vec![],
        };
        ModelRequest {
            system: "s".into(),
            messages: vec![user, assistant, tool_media, tool_text],
            tools: vec![],
        }
    }

    #[test]
    fn responses_serializes_user_and_tool_images_natively() {
        let store = media_store();
        let body =
            responses_request(&multimodal_request(), "gpt-6-astra", None, Some(&store)).unwrap();
        let input = body["input"].as_array().unwrap();
        // User turn: every image is preserved ahead of the text.
        let user = input
            .iter()
            .find(|item| item["type"] == "message" && item["role"] == "user")
            .unwrap();
        let content = user["content"].as_array().unwrap();
        assert_eq!(content.len(), 3, "two images plus text: {content:#?}");
        assert_eq!(content[0]["type"], "input_image");
        assert_eq!(
            content[0]["image_url"],
            "data:image/png;base64,AQID".to_owned()
        );
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(
            content[1]["image_url"],
            "data:image/png;base64,BAU=".to_owned()
        );
        assert_eq!(content[2]["type"], "input_text");
        assert_eq!(content[2]["text"], "inspect");
        // Tool result: the documented function_call_output array with the
        // terminal text first and the image content part after it.
        let tool = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .unwrap();
        let output = tool["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "input_text");
        assert!(output[0]["text"].as_str().unwrap().contains("shot.png"));
        assert_eq!(output[1]["type"], "input_image");
        assert_eq!(output[1]["image_url"], "data:image/png;base64,BAU=");
        assert_eq!(tool["call_id"], "call-media");
    }

    #[test]
    fn anthropic_serializes_image_blocks_in_user_and_tool_result() {
        let store = media_store();
        let body = anthropic_request_with_config(
            &multimodal_request(),
            "claude-opus-5",
            AnthropicConfig::default(),
            Some(&store),
        )
        .unwrap();
        let messages = body["messages"].as_array().unwrap();
        // user / assistant / user(tool results)
        assert_eq!(messages.len(), 3, "{messages:#?}");
        let user_blocks = messages[0]["content"].as_array().unwrap();
        assert_eq!(user_blocks[0]["type"], "image");
        assert_eq!(user_blocks[0]["source"]["type"], "base64");
        assert_eq!(user_blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(user_blocks[0]["source"]["data"], "AQID");
        assert_eq!(user_blocks[1]["type"], "image");
        assert_eq!(user_blocks[2]["type"], "text");
        assert_eq!(user_blocks[2]["text"], "inspect");
        // The tool transaction stays one user turn with tool_result blocks.
        let tool_turn = messages[2]["content"].as_array().unwrap();
        assert_eq!(tool_turn.len(), 2, "two tool results in one turn");
        assert_eq!(tool_turn[0]["type"], "tool_result");
        assert_eq!(tool_turn[0]["tool_use_id"], "call-media");
        let tool_content = tool_turn[0]["content"].as_array().unwrap();
        assert_eq!(tool_content[0]["type"], "text");
        assert_eq!(tool_content[1]["type"], "image");
        assert_eq!(tool_content[1]["source"]["data"], "BAU=");
        assert_eq!(tool_turn[1]["type"], "tool_result");
        assert_eq!(tool_turn[1]["tool_use_id"], "call-text");
        assert_eq!(tool_turn[1]["content"], "contents");
    }

    #[test]
    fn anthropic_merges_user_images_without_losing_blocks() {
        let store = media_store();
        let mut request = multimodal_request();
        request.messages.insert(
            1,
            ModelMessage {
                role: "user".into(),
                content: "kernel context".into(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                reasoning: vec![],
                media: vec![image_ref("img-a")],
            },
        );
        let body = anthropic_request_with_config(
            &request,
            "claude-opus-5",
            AnthropicConfig::default(),
            Some(&store),
        )
        .unwrap();
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        let types: Vec<&str> = blocks
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["image", "image", "text", "image", "text"]);
        // Each source message keeps image-before-text ordering and no block is
        // dropped by the merge.
        assert_eq!(
            blocks
                .iter()
                .filter(|block| block["type"] == "image")
                .count(),
            3
        );
    }

    #[test]
    fn chat_completions_serializes_user_images_and_never_splits_a_tool_transaction() {
        let store = media_store();
        let body = openai_request_full(
            &multimodal_request(),
            "deepseek-flash",
            ReasoningReplay::Replay,
            None,
            None,
            Some(&store),
        )
        .unwrap();
        let messages = body["messages"].as_array().unwrap();
        // system / user / assistant(tool_calls) / tool / tool / user(images)
        assert_eq!(messages.len(), 6, "{messages:#?}");
        let content = messages[1]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(
            content[0]["image_url"]["url"],
            "data:image/png;base64,AQID".to_owned()
        );
        assert_eq!(content[2]["type"], "text");
        // Both terminal tool results precede the adjacent image observation.
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-media");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "call-text");
        assert_eq!(messages[5]["role"], "user");
        let observation = messages[5]["content"].as_array().unwrap();
        assert!(
            observation[0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .contains("data:image/png")
        );
        assert_eq!(observation[1]["type"], "text");
        assert!(
            !messages[3]["content"].is_array() && !messages[4]["content"].is_array(),
            "tool roles keep string content on Chat Completions"
        );
    }

    #[test]
    fn text_only_serialization_is_identical_with_and_without_a_media_store() {
        let request = ModelRequest {
            system: "s".into(),
            messages: vec![
                ModelMessage::text("user", "hi"),
                ModelMessage {
                    role: "assistant".into(),
                    content: "working".into(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "a.txt"}),
                    }],
                    tool_call_id: None,
                    reasoning_content: Some("step".into()),
                    reasoning: vec![],
                    media: vec![],
                },
                ModelMessage {
                    role: "tool".into(),
                    content: "contents".into(),
                    tool_calls: vec![],
                    tool_call_id: Some("call-1".into()),
                    reasoning_content: None,
                    reasoning: vec![],
                    media: vec![],
                },
            ],
            tools: vec![],
        };
        let store = media_store();
        assert_eq!(
            openai_request(&request, "m", ReasoningReplay::Replay).unwrap(),
            openai_request_full(
                &request,
                "m",
                ReasoningReplay::Replay,
                None,
                None,
                Some(&store),
            )
            .unwrap()
        );
        assert_eq!(
            responses_request(&request, "m", None, None).unwrap(),
            responses_request(&request, "m", None, Some(&store)).unwrap()
        );
        assert_eq!(
            anthropic_request(&request, "m").unwrap(),
            anthropic_request_with_config(&request, "m", AnthropicConfig::default(), Some(&store),)
                .unwrap()
        );
    }

    #[test]
    fn missing_media_bytes_fail_loudly_instead_of_sending_placeholders() {
        let empty = MemoryMedia(std::collections::HashMap::new());
        let error = responses_request(&multimodal_request(), "m", None, Some(&empty)).unwrap_err();
        assert!(error.to_string().contains("missing media"), "{error}");
        // No resolver at all is an explicit failure, never a silent drop.
        let error = responses_request(&multimodal_request(), "m", None, None).unwrap_err();
        assert!(
            error.to_string().contains("no media artifact store"),
            "{error}"
        );
    }
}
