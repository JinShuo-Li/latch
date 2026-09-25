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

use crate::config::{EffortForm, EffortMapping};
use latch_protocol::ReasoningEffort;

/// One user-defined per-level effort map at the adapter boundary.
pub type EffortMap = BTreeMap<ReasoningEffort, EffortMapping>;

/// The explicit mapped form for one selected effort, when the user configured
/// one. Malformed maps cannot reach here: configuration validation rejects
/// them before a provider is built.
#[must_use]
pub fn mapped_effort(effort: ReasoningEffort, map: &EffortMap) -> Option<EffortForm> {
    map.get(&effort).and_then(|mapping| mapping.form().ok())
}

/// Resolved Chat Completions effort controls. `effort` is the
/// `reasoning_effort` field value and `thinking` is the DeepSeek-family
/// `thinking.type` toggle; either may be absent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatEffort {
    pub effort: Option<String>,
    pub thinking: Option<String>,
}

/// Resolves the Chat Completions wire form. Without a user map the adapter's
/// catalog behavior is preserved exactly; a `value` form replaces the field
/// value, and a `disabled` form emits only the documented off switch.
#[must_use]
pub fn chat_effort_for(
    effort: ReasoningEffort,
    supports_effort: bool,
    neutral_thinking: ThinkingToggle,
    map: &EffortMap,
) -> ChatEffort {
    match mapped_effort(effort, map) {
        Some(EffortForm::Value(value)) => ChatEffort {
            effort: Some(value),
            thinking: neutral_thinking.wire().map(str::to_owned),
        },
        Some(EffortForm::Disabled) => ChatEffort {
            effort: None,
            thinking: Some("disabled".to_owned()),
        },
        // `budget_tokens` is rejected at validation for this transport, so it
        // cannot appear here; fall back to the neutral mapping defensively.
        _ => ChatEffort {
            effort: supports_effort
                .then(|| effort.wire())
                .flatten()
                .map(str::to_owned),
            thinking: neutral_thinking.wire().map(str::to_owned),
        },
    }
}

/// Resolves the Responses `reasoning.effort` field. Only the `value` form is
/// representable on this transport and is rejected at validation otherwise.
#[must_use]
pub fn responses_effort_for(
    effort: ReasoningEffort,
    supports_effort: bool,
    map: &EffortMap,
) -> Option<String> {
    match mapped_effort(effort, map) {
        Some(EffortForm::Value(value)) => Some(value),
        _ => supports_effort
            .then(|| effort.wire())
            .flatten()
            .map(str::to_owned),
    }
}

/// Resolved Anthropic thinking control at the adapter boundary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AnthropicThinking {
    /// Emit nothing; the API and the model default apply.
    #[default]
    Default,
    /// Adaptive thinking with an optional explicit `output_config.effort`.
    Adaptive { effort: Option<String> },
    /// Classic extended thinking with an explicit token budget.
    Budget { budget_tokens: u64 },
    /// The documented off switch (`thinking.type = "disabled"`).
    Disabled,
}

/// Resolves the Messages wire form. Adaptive models speak
/// `output_config.effort`; classic models speak `thinking.budget_tokens`.
/// Without a user map the existing catalog behavior is preserved exactly.
#[must_use]
pub fn anthropic_thinking_for(
    effort: ReasoningEffort,
    supports_effort: bool,
    adaptive: bool,
    map: &EffortMap,
) -> AnthropicThinking {
    match mapped_effort(effort, map) {
        Some(EffortForm::Value(value)) if adaptive => AnthropicThinking::Adaptive {
            effort: Some(value),
        },
        Some(EffortForm::BudgetTokens(budget_tokens)) if !adaptive => {
            AnthropicThinking::Budget { budget_tokens }
        }
        Some(EffortForm::Disabled) => AnthropicThinking::Disabled,
        // A form this model's thinking mode cannot express is rejected at
        // validation; falling through keeps a hand-built adapter harmless.
        _ if adaptive => AnthropicThinking::Adaptive {
            effort: supports_effort
                .then(|| effort.wire())
                .flatten()
                .map(str::to_owned),
        },
        _ => AnthropicThinking::Default,
    }
}

/// User-Agent sent with every provider request. Keep in sync with the workspace version.
pub const USER_AGENT: &str = "latch/0.2.3";
const OPENCODE_GO_BASE: &str = "https://opencode.ai/zen/go";
const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// Tool-outcome envelope for provider transports that define no error field on
/// a tool result (OpenAI-compatible Chat Completions and the Responses API).
///
/// Adapter-local by design: this is a wire-format workaround, not a kernel
/// concept. The kernel's typed status stays in `ModelMessage::is_error`, and
/// providers with a native signal (Anthropic's `is_error`) never see it. The
/// envelope is deterministic and always the first line, so the model can read
/// the outcome without parsing arbitrary command prose, and the original tool
/// output follows unmodified.
const TOOL_STATUS_OK: &str = "[latch:tool:ok]\n";
const TOOL_STATUS_ERROR: &str = "[latch:tool:error]\n";

/// Renders `content` for a transport with no native tool-error field, prefixing
/// the stable status envelope. Non-tool messages are returned unchanged.
fn tool_status_envelope(content: &str, is_error: bool) -> String {
    let status = if is_error {
        TOOL_STATUS_ERROR
    } else {
        TOOL_STATUS_OK
    };
    format!("{status}{content}")
}

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
    effort_map: EffortMap,
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
            effort_map: EffortMap::new(),
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
    /// Applies the explicit per-level wire map for a Custom/Advanced model.
    #[must_use]
    pub fn with_effort_map(mut self, effort_map: EffortMap) -> Self {
        self.effort_map = effort_map;
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
            effort_map: self.effort_map.clone(),
            media: self.media.clone(),
        }))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let controls = chat_effort_for(
            self.effort,
            self.supports_effort,
            self.thinking,
            &self.effort_map,
        );
        let body = openai_request_full(
            &request,
            &self.model,
            self.reasoning,
            controls.effort.as_deref(),
            controls.thinking.as_deref(),
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
    effort_map: EffortMap,
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
            effort_map: EffortMap::new(),
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
    /// Applies the explicit per-level wire map for a Custom/Advanced model.
    #[must_use]
    pub fn with_effort_map(mut self, effort_map: EffortMap) -> Self {
        self.effort_map = effort_map;
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
            effort_map: self.effort_map.clone(),
            media: self.media.clone(),
        }))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let effort = responses_effort_for(self.effort, self.supports_effort, &self.effort_map);
        let body = responses_request(
            &request,
            &self.model,
            effort.as_deref(),
            self.media.as_deref(),
        )?;
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
                // `function_call_output` carries only `output`, with no error
                // field, so the status travels in the same content envelope the
                // Chat Completions adapter uses.
                let output = tool_status_envelope(&message.content, message.is_error);
                if message.media.is_empty() {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": message.tool_call_id.clone().unwrap_or_default(),
                        "output": output,
                    }));
                } else {
                    // Documented array form: text and images as input content
                    // parts, so the terminal tool transaction stays valid.
                    let mut parts = vec![json!({"type": "input_text", "text": output})];
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
    effort_map: EffortMap,
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
            effort_map: EffortMap::new(),
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
    /// Applies the explicit per-level wire map for a Custom/Advanced model.
    #[must_use]
    pub fn with_effort_map(mut self, effort_map: EffortMap) -> Self {
        self.effort_map = effort_map;
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
            effort_map: self.effort_map.clone(),
            media: self.media.clone(),
        }))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let thinking = anthropic_thinking_for(
            self.effort,
            self.supports_effort,
            self.adaptive_thinking,
            &self.effort_map,
        );
        let config = AnthropicConfig { thinking };
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

/// Resolved Gemini thinking control at the adapter boundary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GeminiThinking {
    /// Emit no thinking fields; the model's own default applies.
    #[default]
    Default,
    /// `thinkingConfig.thinkingLevel` (`minimal`, `low`, `medium`, `high`).
    Level(String),
    /// `thinkingConfig.thinkingBudget` token count; zero is the documented
    /// off switch.
    Budget(u64),
}

/// Resolves the Gemini wire form. Neutral levels map onto the documented
/// `ThinkingLevel` enum, `none` maps onto the documented zero-budget off
/// switch, and a user map takes precedence field for field.
#[must_use]
pub fn gemini_thinking_for(
    effort: ReasoningEffort,
    supports_effort: bool,
    map: &EffortMap,
) -> GeminiThinking {
    match mapped_effort(effort, map) {
        Some(EffortForm::Value(value)) => GeminiThinking::Level(value),
        Some(EffortForm::BudgetTokens(budget)) => GeminiThinking::Budget(budget),
        Some(EffortForm::Disabled) => GeminiThinking::Budget(0),
        _ if supports_effort => match effort {
            ReasoningEffort::None => GeminiThinking::Budget(0),
            ReasoningEffort::Minimal => GeminiThinking::Level("minimal".into()),
            ReasoningEffort::Low => GeminiThinking::Level("low".into()),
            ReasoningEffort::Medium => GeminiThinking::Level("medium".into()),
            ReasoningEffort::High => GeminiThinking::Level("high".into()),
            // Gemini 3 exposes no documented `xhigh`/`max` level; never invent
            // one, and let the model default apply instead.
            _ => GeminiThinking::Default,
        },
        _ => GeminiThinking::Default,
    }
}

/// Maps Gemini `usageMetadata`. Cache reads and thought tokens stay explicit;
/// `output_tokens` includes thoughts, matching the API's billing categories.
#[must_use]
pub fn gemini_usage(usage: &Value) -> Option<Usage> {
    let input = usage.get("promptTokenCount").and_then(Value::as_u64)?;
    let candidates = usage
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let thoughts = usage
        .get("thoughtsTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = usage.get("cachedContentTokenCount").and_then(Value::as_u64);
    Some(Usage {
        input_tokens: input,
        output_tokens: candidates.saturating_add(thoughts),
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        cache_miss_tokens: cache_read.map(|read| input.saturating_sub(read)),
        reasoning_tokens: (thoughts > 0).then_some(thoughts),
    })
}

/// Encodes one durable reference as raw base64 for a Gemini `inlineData` blob.
fn gemini_inline_data(
    media_ref: &MediaRef,
    store: Option<&dyn MediaBytesProvider>,
) -> Result<Value> {
    let store = store.ok_or_else(|| {
        anyhow!("image input is unavailable: no media artifact store is attached to this provider")
    })?;
    let bytes = store.read(media_ref)?;
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
    Ok(json!({
        "inlineData": {
            "mimeType": media_ref.mime_type,
            "data": encoded,
        }
    }))
}

/// Serializes one provider-neutral request into the Gemini `generateContent`
/// shape. Thought, signed-text, and tool-call replay artifacts keep their exact
/// order; tool results correlate by call id, with the function name recovered
/// from the matching assistant call because the API requires it on the wire.
pub fn gemini_request(
    request: &ModelRequest,
    thinking: &GeminiThinking,
    media: Option<&dyn MediaBytesProvider>,
) -> Result<Value> {
    // Tool results carry only the call id. Gemini requires the function name
    // too, so it is recovered from the matching assistant tool call without
    // using the name as the correlation key.
    let mut call_names: BTreeMap<&str, &str> = BTreeMap::new();
    for message in &request.messages {
        for call in &message.tool_calls {
            if !call.id.is_empty() {
                call_names.insert(call.id.as_str(), call.name.as_str());
            }
        }
    }
    let mut contents: Vec<Value> = Vec::new();
    let push_user = |contents: &mut Vec<Value>, parts: Vec<Value>| {
        if let Some(last) = contents.last_mut()
            && last.get("role").and_then(Value::as_str) == Some("user")
            && let Some(existing) = last.get_mut("parts").and_then(Value::as_array_mut)
        {
            existing.extend(parts);
            return;
        }
        contents.push(json!({"role": "user", "parts": parts}));
    };
    let mut index = 0;
    while index < request.messages.len() {
        let message = &request.messages[index];
        match message.role.as_str() {
            "assistant" => {
                contents.push(json!({
                    "role": "model",
                    "parts": gemini_assistant_parts(message),
                }));
                index += 1;
            }
            "tool" => {
                let mut parts = Vec::new();
                while index < request.messages.len() && request.messages[index].role == "tool" {
                    let tool = &request.messages[index];
                    let call_id = tool.tool_call_id.clone().unwrap_or_default();
                    let name = call_names.get(call_id.as_str()).copied();
                    let name = name.ok_or_else(|| {
                        anyhow!(
                            "tool result {} has no matching function call in history",
                            call_id
                        )
                    })?;
                    let response = if tool.is_error {
                        json!({"error": tool.content})
                    } else {
                        json!({"result": tool.content})
                    };
                    let mut function_response = json!({"name": name, "response": response});
                    if !call_id.is_empty() {
                        function_response["id"] = json!(call_id);
                    }
                    // Multimodal results are `FunctionResponse.parts`, not
                    // sibling parts beside the function response.
                    if !tool.media.is_empty() {
                        let mut media_parts = Vec::new();
                        for media_ref in &tool.media {
                            media_parts.push(gemini_inline_data(media_ref, media)?);
                        }
                        function_response["parts"] = Value::Array(media_parts);
                    }
                    parts.push(json!({"functionResponse": function_response}));
                    index += 1;
                }
                push_user(&mut contents, parts);
            }
            _ => {
                let mut parts = Vec::new();
                for media_ref in &message.media {
                    parts.push(gemini_inline_data(media_ref, media)?);
                }
                if !message.content.is_empty() || parts.is_empty() {
                    parts.push(json!({"text": message.content}));
                }
                push_user(&mut contents, parts);
                index += 1;
            }
        }
    }
    // The GenerateContent API nests every thinking control under
    // `generationConfig.thinkingConfig`; `includeThoughts` is the summary
    // switch, and the level/budget controls are mutually exclusive per model.
    let mut thinking_config = json!({"includeThoughts": true});
    match thinking {
        GeminiThinking::Default => {}
        GeminiThinking::Level(level) if !level.trim().is_empty() => {
            thinking_config["thinkingLevel"] = json!(level);
        }
        GeminiThinking::Level(_) => {}
        GeminiThinking::Budget(budget) => {
            thinking_config["thinkingBudget"] = json!(budget);
        }
    }
    let mut body = json!({
        "systemInstruction": {"parts": [{"text": request.system}]},
        "contents": contents,
        "generationConfig": {"thinkingConfig": thinking_config},
    });
    if !request.tools.is_empty() {
        body["tools"] = json!([{
            "functionDeclarations": request.tools.iter().map(|tool| json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })).collect::<Vec<_>>(),
        }]);
    }
    Ok(body)
}

/// One assistant turn's parts in exact original order. Replay artifacts carry
/// the position; calls are referenced by durable id so parallel and same-name
/// calls stay distinguishable.
fn gemini_assistant_parts(message: &latch_protocol::ModelMessage) -> Vec<Value> {
    let mut parts = Vec::new();
    let mut visible_text = false;
    let mut emitted_calls: BTreeSet<&str> = BTreeSet::new();
    for artifact in &message.reasoning {
        match artifact {
            latch_protocol::ReasoningArtifact::Thinking { text, signature } => {
                let mut part = json!({"text": text, "thought": true});
                if !signature.is_empty() {
                    part["thoughtSignature"] = json!(signature);
                }
                parts.push(part);
            }
            latch_protocol::ReasoningArtifact::SignedText { text, signature } => {
                visible_text = true;
                let mut part = json!({"text": text});
                if !signature.is_empty() {
                    part["thoughtSignature"] = json!(signature);
                }
                parts.push(part);
            }
            latch_protocol::ReasoningArtifact::ToolCall { call_id, signature } => {
                let Some(call) = message.tool_calls.iter().find(|call| call.id == *call_id) else {
                    continue;
                };
                let mut part = json!({"functionCall": gemini_function_call(call)});
                if !signature.is_empty() {
                    part["thoughtSignature"] = json!(signature);
                }
                parts.push(part);
                emitted_calls.insert(call.id.as_str());
            }
            // Artifacts owned by other transports are never replayed here.
            _ => {}
        }
    }
    if !visible_text && !message.content.is_empty() {
        parts.push(json!({"text": message.content}));
    }
    for call in &message.tool_calls {
        if call.id.is_empty() || !emitted_calls.contains(call.id.as_str()) {
            parts.push(json!({"functionCall": gemini_function_call(call)}));
        }
    }
    if parts.is_empty() {
        parts.push(json!({"text": ""}));
    }
    parts
}

fn gemini_function_call(call: &ToolCall) -> Value {
    let mut value = json!({"name": call.name, "args": call.arguments});
    if !call.id.is_empty() {
        value["id"] = json!(call.id);
    }
    value
}

/// Incremental Gemini stream state. Parts keep their streamed order; text
/// deltas continue the current part, a signature closes it, and a signature
/// that arrives in its own empty part is preserved at that exact position.
#[derive(Default)]
struct GeminiStreamState {
    parts: Vec<GeminiStreamPart>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    synthesized_calls: usize,
}

#[derive(Default, Clone)]
struct GeminiStreamPart {
    thought: bool,
    text: String,
    signature: String,
    call: Option<ToolCall>,
    /// Function-call arguments seen so far when the API streams them as a JSON
    /// string rather than a complete object.
    pending_args: String,
    /// True once the call arguments are a complete object.
    args_complete: bool,
}

impl GeminiStreamState {
    fn apply(&mut self, value: &Value) -> Result<Vec<StreamEvent>, String> {
        let mut events = Vec::new();
        if let Some(error) = value.get("error") {
            let detail = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("stream error");
            return Err(format!("Gemini stream error: {detail}"));
        }
        if let Some(reason) = value
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
        {
            return Err(format!("Gemini blocked the prompt: {reason}"));
        }
        if let Some(candidate) = value.pointer("/candidates/0") {
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_owned());
            }
            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
            {
                for part in parts {
                    self.apply_part(part, &mut events)?;
                }
            }
        }
        if let Some(usage) = value.get("usageMetadata").and_then(gemini_usage) {
            self.usage = Some(usage);
        }
        Ok(events)
    }

    fn apply_part(&mut self, part: &Value, events: &mut Vec<StreamEvent>) -> Result<(), String> {
        let signature = part
            .get("thoughtSignature")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(function_call) = part.get("functionCall") {
            let name = function_call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let native_id = function_call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let index = if native_id.is_empty() {
                // An id-less fragment continues the previous same-name call only
                // while its arguments are still incomplete; a complete call
                // starts a new part so parallel calls stay distinguishable.
                self.parts.iter().rposition(|part| {
                    part.call
                        .as_ref()
                        .is_some_and(|call| call.name == name && !part.args_complete)
                })
            } else {
                self.parts
                    .iter()
                    .position(|part| part.call.as_ref().is_some_and(|call| call.id == native_id))
            };
            let index = match index {
                Some(index) => index,
                None => {
                    self.synthesized_calls += 1;
                    // Preserve provider-native ids; synthesize a deterministic
                    // fallback only when the API genuinely omits one.
                    let id = if native_id.is_empty() {
                        format!("gemini-call-{}", self.synthesized_calls)
                    } else {
                        native_id
                    };
                    self.parts.push(GeminiStreamPart {
                        call: Some(ToolCall {
                            id,
                            name: name.clone(),
                            arguments: Value::Object(Default::default()),
                        }),
                        ..Default::default()
                    });
                    self.parts.len() - 1
                }
            };
            let target = &mut self.parts[index];
            if let Some(call) = target.call.as_mut() {
                if !name.is_empty() {
                    call.name = name;
                }
                match function_call.get("args") {
                    Some(Value::Object(_)) => {
                        call.arguments = function_call["args"].clone();
                        target.args_complete = true;
                    }
                    Some(Value::String(fragment)) => {
                        target.pending_args.push_str(fragment);
                        if let Ok(parsed) = serde_json::from_str::<Value>(&target.pending_args)
                            && parsed.is_object()
                        {
                            call.arguments = parsed;
                            target.args_complete = true;
                        }
                    }
                    _ => {
                        call.arguments = Value::Object(Default::default());
                        target.args_complete = true;
                    }
                }
            }
            if !signature.is_empty() {
                target.signature = signature.to_owned();
            }
            return Ok(());
        }
        let has_text = part.get("text").is_some();
        let thought = part
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !has_text && signature.is_empty() {
            return Ok(());
        }
        if !has_text {
            // A signature may arrive in its own empty part; attach it to the
            // part it closes when possible, otherwise keep it as its own
            // positional marker.
            if let Some(last) = self.parts.last_mut()
                && last.signature.is_empty()
            {
                last.signature = signature.to_owned();
                return Ok(());
            }
        }
        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
        let can_continue = has_text
            && self.parts.last().is_some_and(|last| {
                last.call.is_none() && last.thought == thought && last.signature.is_empty()
            });
        if !can_continue {
            self.parts.push(GeminiStreamPart {
                thought,
                ..Default::default()
            });
        }
        let target = self.parts.last_mut().expect("part pushed above");
        if has_text {
            target.text.push_str(text);
            if !thought && !text.is_empty() {
                events.push(StreamEvent::TextDelta(text.to_owned()));
            }
        }
        if !signature.is_empty() {
            target.signature = signature.to_owned();
        }
        Ok(())
    }

    fn finish(self) -> Result<ModelResponse, String> {
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut reasoning = Vec::new();
        for mut part in self.parts {
            if let Some(mut call) = part.call.take() {
                if !part.pending_args.is_empty() && call.arguments == json!({}) {
                    call.arguments = serde_json::from_str(&part.pending_args).map_err(|error| {
                        format!(
                            "invalid Gemini function-call arguments for {}: {error}",
                            call.name
                        )
                    })?;
                }
                reasoning.push(latch_protocol::ReasoningArtifact::ToolCall {
                    call_id: call.id.clone(),
                    signature: part.signature,
                });
                tool_calls.push(call);
                continue;
            }
            if part.thought {
                reasoning.push(latch_protocol::ReasoningArtifact::Thinking {
                    text: part.text,
                    signature: part.signature,
                });
            } else {
                // Every visible text part keeps its exact position relative to
                // thought and tool-call parts; the signature is empty for
                // ordinary text.
                text.push_str(&part.text);
                reasoning.push(latch_protocol::ReasoningArtifact::SignedText {
                    text: part.text,
                    signature: part.signature,
                });
            }
        }
        let stop_reason = match self
            .finish_reason
            .as_deref()
            .unwrap_or("STOP")
            .to_ascii_uppercase()
            .as_str()
        {
            "STOP" => "stop".to_owned(),
            "MAX_TOKENS" => "length".to_owned(),
            other => other.to_ascii_lowercase(),
        };
        Ok(ModelResponse {
            text,
            tool_calls,
            stop_reason,
            usage: self.usage,
            reasoning_content: None,
            reasoning,
        })
    }
}

/// Gemini `generateContent` transport. Streams `alt=sse` frames, preserves
/// provider-native function-call ids, and replays ordered thought/signature
/// artifacts exactly as they were received.
pub struct GeminiProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    provider_id: String,
    session_id: Option<Uuid>,
    effort: ReasoningEffort,
    supports_effort: bool,
    replay: ReasoningReplay,
    effort_map: EffortMap,
    media: Option<MediaStore>,
}

impl GeminiProvider {
    #[must_use]
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').into(),
            api_key,
            model,
            provider_id: "gemini".into(),
            session_id: None,
            effort: ReasoningEffort::ProviderDefault,
            supports_effort: false,
            replay: ReasoningReplay::Replay,
            effort_map: EffortMap::new(),
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
        effort: ReasoningEffort,
        supports_effort: bool,
        replay: ReasoningReplay,
    ) -> Self {
        self.effort = effort;
        self.supports_effort = supports_effort;
        self.replay = replay;
        self
    }
    #[must_use]
    pub fn with_effort_map(mut self, effort_map: EffortMap) -> Self {
        self.effort_map = effort_map;
        self
    }
    #[must_use]
    pub fn with_session(mut self, session_id: Uuid) -> Self {
        self.session_id = Some(session_id);
        self
    }
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
impl ModelProvider for GeminiProvider {
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
            replay: self.replay,
            effort_map: self.effort_map.clone(),
            media: self.media.clone(),
        }))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let thinking = if self.replay == ReasoningReplay::Replay {
            gemini_thinking_for(self.effort, self.supports_effort, &self.effort_map)
        } else {
            GeminiThinking::Default
        };
        let body = gemini_request(&request, &thinking, self.media.as_deref())?;
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, self.model
        );
        let response = tokio::select! {
            response = async {
                let mut sent = self.client.post(url).headers(self.request_headers()).json(&body);
                // Zen speaks the gateway's bearer auth; the Google Generative
                // Language API speaks `x-goog-api-key`.
                if is_opencode_go_endpoint(&self.base_url) || is_opencode_zen_endpoint(&self.base_url) {
                    sent = sent.bearer_auth(&self.api_key);
                } else {
                    sent = sent.header("x-goog-api-key", &self.api_key);
                }
                let sent = sent.send().await?;
                checked_response_redacted("gemini", sent, &self.api_key).await
            } => response?,
            () = cancel.cancelled() => bail!("model request cancelled"),
        };
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut state = GeminiStreamState::default();
        loop {
            let next = tokio::select! {()=cancel.cancelled()=>bail!("model request cancelled"),v=bytes.next()=>v};
            let Some(chunk) = next else { break };
            for data in decoder.push(&chunk?) {
                if data == "[DONE]" {
                    continue;
                }
                let value: Value = serde_json::from_str(&data)
                    .map_err(|error| anyhow!("invalid Gemini stream frame: {error}"))?;
                let events = state.apply(&value).map_err(|error| {
                    anyhow!("{}", crate::credentials::redact(&error, &[&self.api_key]))
                })?;
                for event in events {
                    sink(event);
                }
            }
        }
        let result = state.finish().map_err(|error| anyhow!("{error}"))?;
        sink(StreamEvent::Completed(result.clone()));
        Ok(result)
    }
}

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

/// Zen serves both OpenAI-compatible and Gemini transports from one base. Only
/// auth and the stable session header depend on this boundary.
fn is_opencode_zen_endpoint(base_url: &str) -> bool {
    match base_url
        .trim_end_matches('/')
        .strip_prefix("https://opencode.ai/zen")
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
            // Chat Completions gives a tool message no field to carry failure,
            // and inventing one would be rejected by strict endpoints, so the
            // status travels in the content envelope instead.
            "content": tool_status_envelope(&message.content, message.is_error),
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
#[derive(Debug, Clone, Default)]
pub struct AnthropicConfig {
    /// Resolved thinking control: adaptive effort, classic budget, the
    /// documented off switch, or nothing at all.
    pub thinking: AnthropicThinking,
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
                        // Anthropic has a native tool-result error signal, so the
                        // kernel's typed status maps onto the wire directly and
                        // the model never has to read failure out of the output.
                        "is_error": tool.is_error,
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
    // A classic budget must leave room for the visible answer above it.
    let max_tokens = match &config.thinking {
        AnthropicThinking::Adaptive { .. } => 32_000,
        AnthropicThinking::Budget { budget_tokens } => budget_tokens.saturating_add(8_192),
        AnthropicThinking::Default | AnthropicThinking::Disabled => 8_192,
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
    match &config.thinking {
        AnthropicThinking::Default => {}
        AnthropicThinking::Adaptive { effort } => {
            body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
            if let Some(effort) = effort {
                body["output_config"] = json!({"effort": effort});
            }
        }
        AnthropicThinking::Budget { budget_tokens } => {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget_tokens});
        }
        AnthropicThinking::Disabled => {
            body["thinking"] = json!({"type": "disabled"});
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
                    is_error: false,
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
                    is_error: false,
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
        // Chat Completions has no tool-error field, so the kernel's success
        // status is carried in the Latch-owned content envelope ahead of the
        // unmodified output.
        assert_eq!(tool["content"], "[latch:tool:ok]\ncontents");
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
                    is_error: false,
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
                    is_error: false,
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
                    is_error: false,
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

    /// An assistant turn proposing one `read_file` call.
    fn tool_call_turn(call_id: &str) -> ModelMessage {
        ModelMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: call_id.into(),
                name: "read_file".into(),
                arguments: json!({"path": "a.txt"}),
            }],
            tool_call_id: None,
            reasoning_content: None,
            reasoning: vec![],
            media: Vec::new(),
            is_error: false,
        }
    }

    /// One tool-result request with `output` and the kernel's outcome flag.
    fn tool_result_request(call_id: &str, output: &str, is_error: bool) -> ModelRequest {
        ModelRequest {
            system: "s".into(),
            messages: vec![
                ModelMessage::text("user", "check"),
                tool_call_turn(call_id),
                ModelMessage::tool_result(call_id, output, is_error, vec![]),
            ],
            tools: vec![],
        }
    }

    #[test]
    fn anthropic_marks_failed_tool_results_with_the_native_error_field() {
        let body = anthropic_request(
            &tool_result_request("call-1", "exit code 1\nboom", true),
            "m",
        )
        .unwrap();
        let block = &body["messages"][2]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "call-1");
        // Anthropic has a native signal, so the model reads failure from the
        // wire rather than from the wording of the command output.
        assert_eq!(block["is_error"], true);
        assert_eq!(block["content"], "exit code 1\nboom");
    }

    #[test]
    fn anthropic_successful_tool_results_are_not_marked_failed() {
        let body = anthropic_request(&tool_result_request("call-1", "3 tests passed", false), "m")
            .unwrap();
        let block = &body["messages"][2]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["is_error"], false);
        assert_eq!(block["content"], "3 tests passed");
    }

    #[test]
    fn openai_tool_messages_make_failure_visible_at_the_wire_level() {
        // Identical tool output text, opposite kernel outcomes: the two wire
        // messages must still be unambiguously different.
        let ok = openai_request(
            &tool_result_request("call-1", "same output text", false),
            "m",
            ReasoningReplay::Omit,
        )
        .unwrap();
        let failed = openai_request(
            &tool_result_request("call-1", "same output text", true),
            "m",
            ReasoningReplay::Omit,
        )
        .unwrap();
        // 0 = system, 1 = user, 2 = assistant call, 3 = the tool result.
        let ok_tool = &ok["messages"][3];
        let failed_tool = &failed["messages"][3];
        assert_eq!(ok_tool["role"], "tool");
        assert_eq!(failed_tool["role"], "tool");
        assert_ne!(
            ok_tool["content"], failed_tool["content"],
            "success and failure must differ at the wire level"
        );
        assert_eq!(ok_tool["content"], "[latch:tool:ok]\nsame output text");
        assert_eq!(
            failed_tool["content"],
            "[latch:tool:error]\nsame output text"
        );
        // The documented fallback envelope is used precisely because Chat
        // Completions has no such field: none may be invented.
        assert!(failed_tool.get("is_error").is_none());
        assert!(ok_tool.get("is_error").is_none());
        assert_eq!(failed_tool["tool_call_id"], "call-1");
    }

    #[test]
    fn responses_tool_output_carries_the_status_envelope_including_with_images() {
        // The stateless Responses transport has no tool-error field either, so
        // the status must survive both the plain and the multimodal form.
        let plain = responses_request(
            &tool_result_request("call-1", "boom", true),
            "m",
            None,
            None,
        )
        .unwrap();
        let output = plain["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("function_call_output present");
        assert_eq!(output["call_id"], "call-1");
        assert_eq!(output["output"], "[latch:tool:error]\nboom");

        // Multimodal form: the envelope leads the text part, images follow, and
        // the terminal tool transaction stays intact.
        let mut request = tool_result_request("call-1", "boom", true);
        request.messages[2] =
            ModelMessage::tool_result("call-1", "boom", true, vec![image_ref("img-a")]);
        let store = media_store();
        let multimodal = responses_request(&request, "m", None, Some(&store)).unwrap();
        let output = multimodal["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("function_call_output present");
        let parts = output["output"].as_array().expect("array content form");
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[0]["text"], "[latch:tool:error]\nboom");
        assert!(
            parts.iter().any(|part| part["type"] == "input_image"),
            "tool images survive alongside the status envelope"
        );
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
            "latch/0.2.3"
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
                    is_error: false,
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
                    is_error: false,
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
                    is_error: false,
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
                    is_error: false,
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
                    is_error: false,
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
                    is_error: false,
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
                thinking: AnthropicThinking::Adaptive {
                    effort: Some("high".into()),
                },
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

    fn effort_map(entries: &[(ReasoningEffort, EffortMapping)]) -> EffortMap {
        entries.iter().cloned().collect()
    }

    fn value(value: &str) -> EffortMapping {
        EffortMapping {
            value: Some(value.into()),
            ..Default::default()
        }
    }

    fn disabled() -> EffortMapping {
        EffortMapping {
            disabled: Some(true),
            ..Default::default()
        }
    }

    fn budget(budget_tokens: u64) -> EffortMapping {
        EffortMapping {
            budget_tokens: Some(budget_tokens),
            ..Default::default()
        }
    }

    #[test]
    fn chat_transport_emits_the_configured_effort_mapping() {
        // A value mapping replaces the wire value; the family toggle is the
        // neutral one the registry resolved.
        let controls = chat_effort_for(
            ReasoningEffort::High,
            true,
            ThinkingToggle::Default,
            &effort_map(&[(ReasoningEffort::High, value("4"))]),
        );
        assert_eq!(controls.effort.as_deref(), Some("4"));
        assert_eq!(controls.thinking, None);
        let body = openai_request_full(
            &ModelRequest {
                system: "s".into(),
                messages: vec![ModelMessage::text("user", "hi")],
                tools: vec![],
            },
            "m",
            ReasoningReplay::Omit,
            controls.effort.as_deref(),
            controls.thinking.as_deref(),
            None,
        )
        .unwrap();
        assert_eq!(body["reasoning_effort"], "4");

        // Disabled emits only the documented off switch and no effort field.
        let controls = chat_effort_for(
            ReasoningEffort::Low,
            true,
            ThinkingToggle::Enabled,
            &effort_map(&[(ReasoningEffort::Low, disabled())]),
        );
        assert_eq!(controls.effort, None);
        assert_eq!(controls.thinking.as_deref(), Some("disabled"));

        // Without a map the neutral provider behavior is preserved.
        let controls = chat_effort_for(
            ReasoningEffort::High,
            true,
            ThinkingToggle::Enabled,
            &EffortMap::new(),
        );
        assert_eq!(controls.effort.as_deref(), Some("high"));
        assert_eq!(controls.thinking.as_deref(), Some("enabled"));
        let controls = chat_effort_for(
            ReasoningEffort::ProviderDefault,
            true,
            ThinkingToggle::Default,
            &EffortMap::new(),
        );
        assert_eq!(controls.effort, None);
        assert_eq!(controls.thinking, None);
    }

    #[test]
    fn responses_transport_emits_the_configured_effort_mapping() {
        let mapped = responses_effort_for(
            ReasoningEffort::Medium,
            true,
            &effort_map(&[(ReasoningEffort::Medium, value("custom-medium"))]),
        );
        assert_eq!(mapped.as_deref(), Some("custom-medium"));
        let body = responses_request(
            &ModelRequest {
                system: "s".into(),
                messages: vec![ModelMessage::text("user", "hi")],
                tools: vec![],
            },
            "m",
            mapped.as_deref(),
            None,
        )
        .unwrap();
        assert_eq!(body["reasoning"]["effort"], "custom-medium");
        // Built-in models without a map keep the neutral wire value.
        assert_eq!(
            responses_effort_for(ReasoningEffort::XHigh, true, &EffortMap::new()).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            responses_effort_for(ReasoningEffort::ProviderDefault, true, &EffortMap::new()),
            None
        );
    }

    #[test]
    fn anthropic_classic_budget_mapping_serializes_a_thinking_budget() {
        let config = AnthropicConfig {
            thinking: anthropic_thinking_for(
                ReasoningEffort::High,
                true,
                false,
                &effort_map(&[(ReasoningEffort::High, budget(4_096))]),
            ),
        };
        assert_eq!(
            config.thinking,
            AnthropicThinking::Budget {
                budget_tokens: 4_096
            }
        );
        let body = anthropic_request_with_config(
            &ModelRequest {
                system: "s".into(),
                messages: vec![ModelMessage::text("user", "hi")],
                tools: vec![],
            },
            "claude-haiku-4-5",
            config,
            None,
        )
        .unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4_096);
        assert!(body["max_tokens"].as_u64().unwrap() > 4_096);

        // The documented off switch is representable for classic thinking.
        let config = AnthropicConfig {
            thinking: anthropic_thinking_for(
                ReasoningEffort::Low,
                true,
                false,
                &effort_map(&[(ReasoningEffort::Low, disabled())]),
            ),
        };
        let body = anthropic_request_with_config(
            &ModelRequest {
                system: "s".into(),
                messages: vec![ModelMessage::text("user", "hi")],
                tools: vec![],
            },
            "claude-haiku-4-5",
            config,
            None,
        )
        .unwrap();
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn anthropic_adaptive_mapping_replaces_or_disables_output_effort() {
        let config = AnthropicConfig {
            thinking: anthropic_thinking_for(
                ReasoningEffort::Max,
                true,
                true,
                &effort_map(&[(ReasoningEffort::Max, value("custom-max"))]),
            ),
        };
        let body = anthropic_request_with_config(
            &ModelRequest {
                system: "s".into(),
                messages: vec![ModelMessage::text("user", "hi")],
                tools: vec![],
            },
            "claude-opus-5",
            config,
            None,
        )
        .unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "custom-max");

        let config = AnthropicConfig {
            thinking: anthropic_thinking_for(
                ReasoningEffort::Low,
                true,
                true,
                &effort_map(&[(ReasoningEffort::Low, disabled())]),
            ),
        };
        let body = anthropic_request_with_config(
            &ModelRequest {
                system: "s".into(),
                messages: vec![ModelMessage::text("user", "hi")],
                tools: vec![],
            },
            "claude-opus-5",
            config,
            None,
        )
        .unwrap();
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("output_config").is_none());
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
            is_error: false,
            content: "inspect".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning: vec![],
            media: vec![image_ref("img-a"), image_ref("img-b")],
        };
        let assistant = ModelMessage {
            role: "assistant".into(),
            is_error: false,
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
            is_error: false,
            content: "image: [image: shot.png · 1440×900]".into(),
            tool_calls: vec![],
            tool_call_id: Some("call-media".into()),
            reasoning_content: None,
            reasoning: vec![],
            media: vec![image_ref("img-b")],
        };
        let tool_text = ModelMessage {
            role: "tool".into(),
            is_error: false,
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
                is_error: false,
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
                    is_error: false,
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
                    is_error: false,
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

    #[test]
    fn gemini_thinking_maps_neutral_levels_and_user_forms() {
        assert_eq!(
            gemini_thinking_for(ReasoningEffort::Low, true, &EffortMap::new()),
            GeminiThinking::Level("low".into())
        );
        assert_eq!(
            gemini_thinking_for(ReasoningEffort::None, true, &EffortMap::new()),
            GeminiThinking::Budget(0)
        );
        // Provider default omits thinking fields entirely; the model decides.
        assert_eq!(
            gemini_thinking_for(ReasoningEffort::ProviderDefault, true, &EffortMap::new()),
            GeminiThinking::Default
        );
        // No documented Gemini level for xhigh/max: never invent one.
        assert_eq!(
            gemini_thinking_for(ReasoningEffort::Max, true, &EffortMap::new()),
            GeminiThinking::Default
        );
        // A configured map wins field for field.
        assert_eq!(
            gemini_thinking_for(
                ReasoningEffort::High,
                true,
                &effort_map(&[(ReasoningEffort::High, value("high"))])
            ),
            GeminiThinking::Level("high".into())
        );
        assert_eq!(
            gemini_thinking_for(
                ReasoningEffort::Low,
                true,
                &effort_map(&[(ReasoningEffort::Low, budget(8_192))])
            ),
            GeminiThinking::Budget(8_192)
        );
        assert_eq!(
            gemini_thinking_for(
                ReasoningEffort::Medium,
                true,
                &effort_map(&[(ReasoningEffort::Medium, disabled())])
            ),
            GeminiThinking::Budget(0)
        );
    }

    #[test]
    fn gemini_request_serializes_system_tools_thinking_and_images() {
        let request = ModelRequest {
            system: "be terse".into(),
            messages: vec![ModelMessage {
                role: "user".into(),
                is_error: false,
                content: "user".into(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                reasoning: vec![],
                media: vec![image_ref("img-1")],
            }],
            tools: vec![ToolDefinition {
                name: "read_file".into(),
                description: "read a file".into(),
                input_schema: json!({"type": "object"}),
            }],
        };
        let store = MemoryMedia(std::collections::HashMap::from([(
            "img-1".to_owned(),
            vec![1_u8, 2, 3],
        )]));
        let body = gemini_request(
            &request,
            &GeminiThinking::Level("high".into()),
            Some(&store),
        )
        .unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be terse");
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[0]["inlineData"]["data"], "AQID");
        assert_eq!(parts[1]["text"], "user");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["name"],
            "read_file"
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        assert!(
            body["generationConfig"]["thinkingConfig"]
                .get("thinkingBudget")
                .is_none()
        );
        assert!(
            body["generationConfig"].get("thinkingLevel").is_none(),
            "thinking controls must stay nested under thinkingConfig"
        );

        let body = gemini_request(
            &ModelRequest {
                system: "s".into(),
                messages: vec![ModelMessage::text("user", "hi")],
                tools: vec![],
            },
            &GeminiThinking::Budget(0),
            None,
        )
        .unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn gemini_request_nests_thinking_config_controls() {
        let request = ModelRequest {
            system: "s".into(),
            messages: vec![ModelMessage::text("user", "hi")],
            tools: vec![],
        };
        // Default thinking still asks for summaries and emits no control.
        let body = gemini_request(&request, &GeminiThinking::Default, None).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": true})
        );
        // A level model carries exactly `thinkingLevel`.
        let body = gemini_request(&request, &GeminiThinking::Level("medium".into()), None).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": true, "thinkingLevel": "medium"})
        );
        // A budget model carries exactly `thinkingBudget`.
        let body = gemini_request(&request, &GeminiThinking::Budget(8_192), None).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": true, "thinkingBudget": 8_192})
        );
        // The documented off switch for a zero-budget model is `thinkingBudget: 0`.
        let body = gemini_request(&request, &GeminiThinking::Budget(0), None).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": true, "thinkingBudget": 0})
        );
        // An empty level is never emitted as a control.
        let body = gemini_request(&request, &GeminiThinking::Level("  ".into()), None).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": true})
        );
        assert_eq!(body["generationConfig"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn gemini_request_replays_ordered_thoughts_calls_and_text() {
        let request = ModelRequest {
            system: "s".into(),
            messages: vec![
                ModelMessage::text("user", "compare Paris and London"),
                ModelMessage {
                    role: "assistant".into(),
                    is_error: false,
                    content: "checking".into(),
                    tool_calls: vec![
                        ToolCall {
                            id: "call-paris".into(),
                            name: "get_current_temperature".into(),
                            arguments: json!({"location": "Paris"}),
                        },
                        ToolCall {
                            id: "call-london".into(),
                            name: "get_current_temperature".into(),
                            arguments: json!({"location": "London"}),
                        },
                    ],
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: vec![
                        latch_protocol::ReasoningArtifact::Thinking {
                            text: "I should check both".into(),
                            signature: "sig-thought".into(),
                        },
                        latch_protocol::ReasoningArtifact::SignedText {
                            text: "checking".into(),
                            signature: String::new(),
                        },
                        latch_protocol::ReasoningArtifact::ToolCall {
                            call_id: "call-paris".into(),
                            signature: "sig-first-call".into(),
                        },
                        latch_protocol::ReasoningArtifact::ToolCall {
                            call_id: "call-london".into(),
                            signature: String::new(),
                        },
                    ],
                    media: Vec::new(),
                },
            ],
            tools: vec![],
        };
        let body = gemini_request(&request, &GeminiThinking::Default, None).unwrap();
        let parts = body["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 4, "{parts:?}");
        assert_eq!(parts[0]["text"], "I should check both");
        assert_eq!(parts[0]["thought"], true);
        assert_eq!(parts[0]["thoughtSignature"], "sig-thought");
        assert_eq!(parts[1]["text"], "checking");
        assert!(parts[1].get("thoughtSignature").is_none());
        assert_eq!(parts[2]["functionCall"]["name"], "get_current_temperature");
        assert_eq!(parts[2]["functionCall"]["id"], "call-paris");
        assert_eq!(parts[2]["functionCall"]["args"]["location"], "Paris");
        assert_eq!(parts[2]["thoughtSignature"], "sig-first-call");
        assert_eq!(parts[3]["functionCall"]["id"], "call-london");
        assert_eq!(parts[3]["functionCall"]["args"]["location"], "London");
        assert!(parts[3].get("thoughtSignature").is_none());
        // The visible text is never duplicated: replayed signed/plain text
        // parts already carry it.
        assert_eq!(
            parts
                .iter()
                .filter(|part| part.get("text") == Some(&json!("checking")))
                .count(),
            1
        );
    }

    #[test]
    fn gemini_request_correlates_tool_results_by_call_id_and_recovers_names() {
        let request = ModelRequest {
            system: "s".into(),
            messages: vec![
                ModelMessage {
                    role: "assistant".into(),
                    is_error: false,
                    content: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "call-1".into(),
                            name: "read_file".into(),
                            arguments: json!({"path": "a.txt"}),
                        },
                        ToolCall {
                            id: "call-2".into(),
                            name: "read_file".into(),
                            arguments: json!({"path": "b.txt"}),
                        },
                    ],
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: vec![],
                    media: Vec::new(),
                },
                ModelMessage::tool_result("call-1", "contents a", false, vec![]),
                ModelMessage::tool_result("call-2", "boom", true, vec![]),
            ],
            tools: vec![],
        };
        let body = gemini_request(&request, &GeminiThinking::Default, None).unwrap();
        let tool_parts = body["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(tool_parts.len(), 2);
        assert_eq!(tool_parts[0]["functionResponse"]["id"], "call-1");
        assert_eq!(tool_parts[0]["functionResponse"]["name"], "read_file");
        assert_eq!(
            tool_parts[0]["functionResponse"]["response"]["result"],
            "contents a"
        );
        assert_eq!(tool_parts[1]["functionResponse"]["id"], "call-2");
        assert_eq!(
            tool_parts[1]["functionResponse"]["response"]["error"],
            "boom"
        );

        // An orphan tool result is a real inconsistency, not a silent drop.
        let orphan = ModelRequest {
            system: "s".into(),
            messages: vec![ModelMessage::tool_result("missing", "x", false, vec![])],
            tools: vec![],
        };
        let error = gemini_request(&orphan, &GeminiThinking::Default, None).unwrap_err();
        assert!(
            error.to_string().contains("no matching function call"),
            "{error}"
        );
    }

    #[test]
    fn gemini_request_nests_tool_media_inside_function_responses() {
        let request = ModelRequest {
            system: "s".into(),
            messages: vec![
                ModelMessage {
                    role: "assistant".into(),
                    is_error: false,
                    content: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "call-image".into(),
                            name: "screenshot".into(),
                            arguments: json!({"target": "window"}),
                        },
                        ToolCall {
                            id: "call-failed".into(),
                            name: "read_file".into(),
                            arguments: json!({"path": "missing.txt"}),
                        },
                    ],
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: vec![],
                    media: Vec::new(),
                },
                ModelMessage::tool_result(
                    "call-image",
                    "captured",
                    false,
                    vec![image_ref("img-1")],
                ),
                ModelMessage::tool_result(
                    "call-failed",
                    "no such file",
                    true,
                    vec![image_ref("img-2")],
                ),
            ],
            tools: vec![],
        };
        let store = MemoryMedia(std::collections::HashMap::from([
            ("img-1".to_owned(), vec![1_u8, 2, 3]),
            ("img-2".to_owned(), vec![4_u8, 5, 6]),
        ]));
        let body = gemini_request(&request, &GeminiThinking::Default, Some(&store)).unwrap();
        let parts = body["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "{parts:#?}");
        // Successful result: media nests inside the function response, and the
        // call id and recovered function name stay on it.
        assert_eq!(
            parts[0],
            json!({
                "functionResponse": {
                    "id": "call-image",
                    "name": "screenshot",
                    "response": {"result": "captured"},
                    "parts": [{"inlineData": {"mimeType": "image/png", "data": "AQID"}}],
                }
            })
        );
        // Failed result: same nesting with the error response.
        assert_eq!(
            parts[1],
            json!({
                "functionResponse": {
                    "id": "call-failed",
                    "name": "read_file",
                    "response": {"error": "no such file"},
                    "parts": [{"inlineData": {"mimeType": "image/png", "data": "BAUG"}}],
                }
            })
        );
        for part in parts {
            assert!(
                part.get("parts").is_none(),
                "media must never sit beside functionResponse: {part}"
            );
        }
    }

    #[test]
    fn gemini_stream_assembles_text_thoughts_signatures_and_parallel_calls() {
        let mut state = GeminiStreamState::default();
        let mut deltas = Vec::new();
        for frame in [
            json!({"candidates": [{"content": {"role": "model", "parts": [
                {"text": "Let me ", "thought": true}
            ]}}]}),
            // The thought signature may arrive in its own empty part.
            json!({"candidates": [{"content": {"role": "model", "parts": [
                {"text": "", "thought": true, "thoughtSignature": "sig-thought"}
            ]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [
                {"text": "Checking "}
            ]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"id": "native-1", "name": "get_temperature", "args": {"location": "Paris"}}},
                {"functionCall": {"id": "native-2", "name": "get_temperature", "args": {"location": "London"}}}
            ]}}]}),
            // A parallel first call's signature may arrive after both calls.
            json!({"candidates": [{"content": {"role": "model", "parts": [
                {"functionCall": {"id": "native-1", "name": "get_temperature", "args": {"location": "Paris"}}, "thoughtSignature": "sig-call"}
            ]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [
                {"text": "weather."}
            ]}, "finishReason": "STOP"}], "usageMetadata": {
                "promptTokenCount": 10, "candidatesTokenCount": 4,
                "thoughtsTokenCount": 6, "cachedContentTokenCount": 2
            }}),
        ] {
            deltas.extend(state.apply(&frame).unwrap());
        }
        assert_eq!(
            deltas,
            vec![
                StreamEvent::TextDelta("Checking ".into()),
                StreamEvent::TextDelta("weather.".into()),
            ],
            "thought text is never emitted as assistant text"
        );
        let response = state.finish().unwrap();
        assert_eq!(response.text, "Checking weather.");
        assert_eq!(response.stop_reason, "stop");
        assert_eq!(
            response.tool_calls,
            vec![
                ToolCall {
                    id: "native-1".into(),
                    name: "get_temperature".into(),
                    arguments: json!({"location": "Paris"}),
                },
                ToolCall {
                    id: "native-2".into(),
                    name: "get_temperature".into(),
                    arguments: json!({"location": "London"}),
                },
            ]
        );
        assert_eq!(
            response.reasoning,
            vec![
                latch_protocol::ReasoningArtifact::Thinking {
                    text: "Let me ".into(),
                    signature: "sig-thought".into(),
                },
                latch_protocol::ReasoningArtifact::SignedText {
                    text: "Checking ".into(),
                    signature: String::new(),
                },
                latch_protocol::ReasoningArtifact::ToolCall {
                    call_id: "native-1".into(),
                    signature: "sig-call".into(),
                },
                latch_protocol::ReasoningArtifact::ToolCall {
                    call_id: "native-2".into(),
                    signature: String::new(),
                },
                latch_protocol::ReasoningArtifact::SignedText {
                    text: "weather.".into(),
                    signature: String::new(),
                },
            ],
            "part order, signatures, and call association are preserved"
        );
        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.reasoning_tokens, Some(6));
        assert_eq!(usage.cache_read_tokens, Some(2));
        assert_eq!(usage.cache_miss_tokens, Some(8));
    }

    #[test]
    fn gemini_stream_synthesizes_ids_only_when_the_api_omits_them() {
        let mut state = GeminiStreamState::default();
        state
            .apply(&json!({"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "read_file", "args": "{\"path\":"}}
            ]}}]}))
            .unwrap();
        state
            .apply(&json!({"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "read_file", "args": "\"a.txt\"}"}}
            ]}}]}))
            .unwrap();
        // A second id-less call gets its own deterministic id.
        state
            .apply(&json!({"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "read_file", "args": {"path": "b.txt"}}}
            ]}}]}))
            .unwrap();
        let response = state.finish().unwrap();
        assert_eq!(
            response
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec!["gemini-call-1", "gemini-call-2"]
        );
        assert_eq!(response.tool_calls[0].arguments, json!({"path": "a.txt"}));
    }

    #[test]
    fn gemini_stream_reports_errors_and_malformed_frames() {
        let mut state = GeminiStreamState::default();
        let error = state
            .apply(&json!({"error": {"code": 400, "message": "bad request"}}))
            .unwrap_err();
        assert!(error.contains("bad request"), "{error}");
        let blocked = GeminiStreamState::default()
            .apply(&json!({"promptFeedback": {"blockReason": "SAFETY"}}))
            .unwrap_err();
        assert!(blocked.contains("blocked"), "{blocked}");

        let mut state = GeminiStreamState::default();
        state
            .apply(&json!({"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "read_file", "args": "not json"}}
            ]}}]}))
            .unwrap();
        let error = state.finish().unwrap_err();
        assert!(
            error.contains("invalid Gemini function-call arguments"),
            "{error}"
        );
    }

    #[test]
    fn gemini_thought_signatures_survive_the_durable_event_round_trip() {
        // Gemini response -> provider-neutral reasoning artifacts.
        let mut state = GeminiStreamState::default();
        for frame in [
            json!({"candidates": [{"content": {"parts": [
                {"text": "thinking", "thought": true, "thoughtSignature": "sig-1"}
            ]}}]}),
            json!({"candidates": [{"content": {"parts": [
                {"functionCall": {"id": "call-1", "name": "read_file", "args": {"path": "a"}}},
                {"functionCall": {"id": "call-2", "name": "read_file", "args": {"path": "b"}}, "thoughtSignature": "sig-2"}
            ]}}]}),
        ] {
            state.apply(&frame).unwrap();
        }
        let response = state.finish().unwrap();

        // -> durable event -> JSON (the SQLite representation) -> resume.
        let event = latch_protocol::EventPayload::AssistantMessageCompleted {
            text: response.text.clone(),
            tool_calls: response.tool_calls.clone(),
            reasoning_content: response.reasoning_content.clone(),
            reasoning: response.reasoning.clone(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: latch_protocol::EventPayload = serde_json::from_str(&json).unwrap();
        let latch_protocol::EventPayload::AssistantMessageCompleted {
            text,
            tool_calls,
            reasoning_content,
            reasoning,
        } = restored
        else {
            panic!("expected assistant message");
        };

        // -> reconstructed request.
        let request = ModelRequest {
            system: "s".into(),
            messages: vec![
                ModelMessage::text("user", "read both"),
                ModelMessage {
                    role: "assistant".into(),
                    is_error: false,
                    content: text,
                    tool_calls,
                    tool_call_id: None,
                    reasoning_content,
                    reasoning,
                    media: Vec::new(),
                },
                ModelMessage::tool_result("call-1", "a contents", false, vec![]),
                ModelMessage::tool_result("call-2", "b contents", false, vec![]),
            ],
            tools: vec![],
        };
        let body = gemini_request(&request, &GeminiThinking::Default, None).unwrap();
        let parts = body["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "thinking");
        assert_eq!(parts[0]["thought"], true);
        assert_eq!(parts[0]["thoughtSignature"], "sig-1");
        assert_eq!(parts[1]["functionCall"]["id"], "call-1");
        assert!(parts[1].get("thoughtSignature").is_none());
        assert_eq!(parts[2]["functionCall"]["id"], "call-2");
        assert_eq!(parts[2]["thoughtSignature"], "sig-2");
        assert_eq!(parts.len(), 3, "no reordering or duplication: {parts:?}");
        let tool_parts = body["contents"][2]["parts"].as_array().unwrap();
        assert_eq!(tool_parts[0]["functionResponse"]["id"], "call-1");
        assert_eq!(tool_parts[1]["functionResponse"]["id"], "call-2");
    }
}
