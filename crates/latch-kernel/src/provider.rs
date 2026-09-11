use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;
use latch_protocol::{ModelRequest, ModelResponse, StreamEvent, ToolCall, Usage};
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

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
}
impl OpenAiProvider {
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
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut text = String::new();
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
        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&anthropic_request(&request, &self.model))
            .send()
            .await?
            .error_for_status()?;
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

fn openai_request(r: &ModelRequest, model: &str) -> Value {
    let mut messages = vec![json!({"role":"system","content":r.system})];
    messages.extend(
        r.messages
            .iter()
            .map(|m| json!({"role":m.role,"content":m.content})),
    );
    json!({"model":model,"messages":messages,"tools":r.tools.iter().map(|t|json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.input_schema}})).collect::<Vec<_>>(),"stream":true,"stream_options":{"include_usage":true}})
}
fn anthropic_request(r: &ModelRequest, model: &str) -> Value {
    json!({"model":model,"max_tokens":8192,"system":r.system,"messages":r.messages,"tools":r.tools.iter().map(|t|json!({"name":t.name,"description":t.description,"input_schema":t.input_schema})).collect::<Vec<_>>(),"stream":true})
}

#[derive(Default)]
struct SseDecoder {
    buffer: String,
}
impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut out = vec![];
        while let Some(i) = self.buffer.find("\n\n") {
            let frame = self.buffer[..i].replace("\r", "");
            self.buffer.drain(..i + 2);
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

#[cfg(test)]
mod tests {
    use super::*;
    use latch_protocol::{ModelMessage, ToolDefinition};
    #[test]
    fn converts_provider_requests() {
        let r = ModelRequest {
            system: "s".into(),
            messages: vec![ModelMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
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
    fn decodes_fragmented_sse() {
        let mut d = SseDecoder::default();
        assert!(d.push(b"data: {\"a\":").is_empty());
        assert_eq!(d.push(b"1}\n\n"), ["{\"a\":1}"]);
    }
}
