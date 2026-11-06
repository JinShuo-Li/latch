use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Behavior {
    NoHeaders,
    Idle,
    Heartbeats,
    LongThinking,
    DropSink,
    Complete,
}
struct Stub {
    behavior: Behavior,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl ModelProvider for Stub {
    fn name(&self) -> &str {
        "stub"
    }
    fn model(&self) -> &str {
        "model"
    }
    fn for_session(&self, _: Uuid) -> Option<Arc<dyn ModelProvider>> {
        Some(Arc::new(Self {
            behavior: self.behavior,
            calls: self.calls.clone(),
        }))
    }
    async fn stream(
        &self,
        _: ModelRequest,
        _: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if matches!(self.behavior, Behavior::NoHeaders) {
            return std::future::pending().await;
        }
        sink(StreamEvent::Activity(StreamActivity::Connected));
        match self.behavior {
            Behavior::Heartbeats => loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                sink(StreamEvent::Activity(StreamActivity::Receiving));
            },
            Behavior::LongThinking => {
                for _ in 0..150 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    sink(StreamEvent::Activity(StreamActivity::Reasoning));
                }
                Ok(ModelResponse::default())
            }
            Behavior::Complete => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(ModelResponse::default())
            }
            Behavior::DropSink => {
                drop(sink);
                std::future::pending().await
            }
            _ => std::future::pending().await,
        }
    }
}
fn request() -> ModelRequest {
    ModelRequest {
        system: String::new(),
        messages: vec![],
        tools: vec![],
    }
}
fn policy() -> ProviderRequestConfig {
    ProviderRequestConfig {
        connect_timeout_seconds: 1,
        headers_timeout_seconds: 3,
        idle_timeout_seconds: 5,
        request_timeout_seconds: 20,
    }
}
fn provider(behavior: Behavior) -> Arc<dyn ModelProvider> {
    BoundedProvider::wrap(
        Arc::new(Stub {
            behavior,
            calls: Arc::default(),
        }),
        policy(),
    )
}

#[tokio::test(start_paused = true)]
async fn headers_and_idle_deadlines_are_distinct_and_drop_the_request() {
    for (behavior, phase, seconds) in [
        (Behavior::NoHeaders, TimeoutPhase::Headers, 3),
        (Behavior::Idle, TimeoutPhase::Idle, 5),
        (Behavior::DropSink, TimeoutPhase::Idle, 5),
    ] {
        let start = Instant::now();
        let error = provider(behavior)
            .stream(request(), CancellationToken::new(), Arc::new(|_| {}))
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProviderTimeout>().unwrap().phase,
            phase
        );
        assert_eq!(start.elapsed(), Duration::from_secs(seconds));
    }
}

#[tokio::test(start_paused = true)]
async fn heartbeats_reset_idle_but_not_the_total_deadline() {
    let start = Instant::now();
    let error = provider(Behavior::Heartbeats)
        .stream(request(), CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ProviderTimeout>().unwrap().phase,
        TimeoutPhase::Request
    );
    assert_eq!(start.elapsed(), Duration::from_secs(20));
    assert!(retry_delay(&error, 0, Uuid::nil()).is_none());
}

#[tokio::test(start_paused = true)]
async fn thinking_can_exceed_180_seconds_but_complete_silence_cannot() {
    let start = Instant::now();
    let thinking = BoundedProvider::wrap(
        Arc::new(Stub {
            behavior: Behavior::LongThinking,
            calls: Arc::default(),
        }),
        ProviderRequestConfig::default(),
    );
    assert!(
        thinking
            .stream(request(), CancellationToken::new(), Arc::new(|_| {}))
            .await
            .is_ok()
    );
    assert_eq!(start.elapsed(), Duration::from_secs(300));

    let start = Instant::now();
    let silent = BoundedProvider::wrap(
        Arc::new(Stub {
            behavior: Behavior::Idle,
            calls: Arc::default(),
        }),
        ProviderRequestConfig::default(),
    );
    let error = silent
        .stream(request(), CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ProviderTimeout>().unwrap().phase,
        TimeoutPhase::Idle
    );
    assert_eq!(start.elapsed(), Duration::from_secs(180));
}

#[tokio::test(start_paused = true)]
async fn cancellation_is_prompt_and_child_rebinding_keeps_deadlines() {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        trigger.cancel();
    });
    let child = provider(Behavior::Idle)
        .for_session(Uuid::new_v4())
        .unwrap();
    assert_eq!(child.name(), "stub");
    assert_eq!(child.model(), "model");
    let start = Instant::now();
    let error = child
        .stream(request(), cancel, Arc::new(|_| {}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert_eq!(start.elapsed(), Duration::from_secs(1));
    let error = child
        .stream(request(), CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ProviderTimeout>().unwrap().phase,
        TimeoutPhase::Idle
    );
}

#[tokio::test(start_paused = true)]
async fn completed_requests_are_unchanged_and_precancel_does_not_start_transport() {
    assert!(
        provider(Behavior::Complete)
            .stream(request(), CancellationToken::new(), Arc::new(|_| {}))
            .await
            .is_ok()
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = BoundedProvider::wrap(
        Arc::new(Stub {
            behavior: Behavior::Complete,
            calls: calls.clone(),
        }),
        policy(),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        provider
            .stream(request(), cancel, Arc::new(|_| {}))
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn only_transient_statuses_retry_with_bounded_backoff() {
    for status in [400, 401, 403, 404, 422, 501] {
        let error = ProviderHttpError {
            status,
            retry_after: None,
            message: "redacted".into(),
        }
        .into();
        assert!(retry_delay(&error, 0, Uuid::nil()).is_none());
    }
    for status in [408, 429, 500, 502, 503, 504] {
        let error = ProviderHttpError {
            status,
            retry_after: None,
            message: "redacted".into(),
        }
        .into();
        assert_eq!(
            retry_delay(&error, 0, Uuid::nil()),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            retry_delay(&error, 1, Uuid::nil()),
            Some(Duration::from_secs(2))
        );
    }
    let error = ProviderHttpError {
        status: 429,
        retry_after: Some(Duration::from_secs(600)),
        message: "redacted".into(),
    }
    .into();
    assert_eq!(
        retry_delay(&error, 0, Uuid::nil()),
        Some(Duration::from_secs(60))
    );
    let error = ProviderHttpError {
        status: 503,
        retry_after: Some(Duration::from_secs(12)),
        message: "redacted".into(),
    }
    .into();
    assert_eq!(
        retry_delay(&error, 0, Uuid::nil()),
        Some(Duration::from_secs(12))
    );
    assert!(retry_delay(&anyhow::anyhow!("unknown"), 0, Uuid::nil()).is_none());
}

#[tokio::test]
async fn protocol_completion_does_not_wait_for_connection_close_and_eof_is_not_success() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for transport in 0..4 {
        for complete in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 4096];
                let mut received = Vec::new();
                loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    received.extend_from_slice(&buffer[..n]);
                    if let Some(end) = received.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&received[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if received.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                if complete {
                    let frame = match transport {
                        0 => {
                            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n"
                        }
                        1 => {
                            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}]}}\n\n"
                        }
                        2 => {
                            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n"
                        }
                        _ => {
                            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\n\n"
                        }
                    };
                    socket
                        .write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes())
                        .await
                        .unwrap();
                    // Intentionally leave the HTTP body open after protocol completion.
                    while socket.read(&mut buffer).await.unwrap() != 0 {}
                } else {
                    // A clean HTTP EOF without a terminal protocol event is
                    // still an interrupted model response, eligible for retry
                    // only when no semantic output was observed.
                    socket.write_all(b"0\r\n\r\n").await.unwrap();
                }
            });
            let provider: Arc<dyn ModelProvider> = match transport {
                0 => Arc::new(crate::provider::OpenAiProvider::new(
                    base,
                    "fixture".into(),
                    "m".into(),
                )),
                1 => Arc::new(crate::provider::OpenAiResponsesProvider::new(
                    base,
                    "fixture".into(),
                    "m".into(),
                )),
                2 => Arc::new(crate::provider::AnthropicProvider::new(
                    base,
                    "fixture".into(),
                    "m".into(),
                )),
                _ => Arc::new(crate::provider::GeminiProvider::new(
                    base,
                    "fixture".into(),
                    "m".into(),
                )),
            };
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                provider.stream(request(), CancellationToken::new(), Arc::new(|_| {})),
            )
            .await
            .expect("terminal event must release the stream");
            if complete {
                assert_eq!(result.unwrap().text, "ok", "transport {transport}");
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.to_string().contains("before completion"),
                    "transport {transport}: {error:#}"
                );
                assert!(retry_delay(&error, 0, Uuid::nil()).is_some());
            }
            tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
        }
    }
}

#[tokio::test]
async fn all_http_adapters_obey_header_and_stream_idle_deadlines() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut cases = Vec::new();
    for transport in 0..4 {
        for connected in [false, true] {
            cases.push(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut buffer = [0; 4096];
                    assert!(socket.read(&mut buffer).await.unwrap() > 0);
                    if connected {
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                    }
                    // A dropped request must release the real connection, not
                    // leave a detached network task running after timeout.
                    while socket.read(&mut buffer).await.unwrap() != 0 {}
                });
                let inner: Arc<dyn ModelProvider> = match transport {
                    0 => Arc::new(crate::provider::OpenAiProvider::new(base, "fixture".into(), "m".into())),
                    1 => Arc::new(crate::provider::OpenAiResponsesProvider::new(base, "fixture".into(), "m".into())),
                    2 => Arc::new(crate::provider::AnthropicProvider::new(base, "fixture".into(), "m".into())),
                    _ => Arc::new(crate::provider::GeminiProvider::new(base, "fixture".into(), "m".into())),
                };
                let bounded = BoundedProvider::wrap(inner, ProviderRequestConfig {
                    connect_timeout_seconds: 1, headers_timeout_seconds: 1, idle_timeout_seconds: 1, request_timeout_seconds: 5,
                });
                let result = tokio::time::timeout(Duration::from_secs(5), bounded.stream(request(), CancellationToken::new(), Arc::new(|_| {}))).await.expect("outer watchdog");
                let error = result.unwrap_err();
                let phase = error.downcast_ref::<ProviderTimeout>().expect("typed timeout").phase;
                assert_eq!(phase, if connected { TimeoutPhase::Idle } else { TimeoutPhase::Headers });
                tokio::time::timeout(Duration::from_secs(2), server).await.expect("connection released").unwrap();
            });
        }
    }
    futures::future::join_all(cases).await;
}

#[tokio::test]
async fn registry_connect_budget_bounds_tls_stalls_and_survives_child_rebinding() {
    use tokio::io::AsyncReadExt;
    let mut cases = Vec::new();
    for transport in [
        "chat_completions",
        "responses",
        "anthropic_messages",
        "gemini",
    ] {
        for child in [false, true] {
            cases.push(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut buffer = [0; 4096];
                    // Accept TCP and the TLS ClientHello, but never complete TLS.
                    assert!(socket.read(&mut buffer).await.unwrap() > 0);
                    loop {
                        match socket.read(&mut buffer).await {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::ConnectionReset
                                        | std::io::ErrorKind::ConnectionAborted
                                ) =>
                            {
                                break;
                            }
                            Err(error) => panic!("TLS fixture read: {error}"),
                        }
                    }
                });
                let text = format!(
                    r#"
                    [provider_request]
                    connect_timeout_seconds = 1
                    headers_timeout_seconds = 5
                    [providers.fixture]
                    kind = "openai-compatible"
                    base_url = "https://{address}"
                    credential = "file:fixture"
                    default_model = "m"
                    [providers.fixture.models.m]
                    transport = "{transport}"
                "#
                );
                let config: crate::config::Config = toml::from_str(&text).unwrap();
                let registry = crate::providers::ProviderRegistry::from_config(&config).unwrap();
                let dir = tempfile::tempdir().unwrap();
                let mut credentials =
                    crate::credentials::CredentialStore::open(dir.path().join("secrets.toml"))
                        .unwrap();
                credentials.set("fixture", "fixture-key").unwrap();
                let (profile, descriptor) = registry
                    .resolve_profile(&latch_protocol::InferenceProfile::new(
                        "fixture",
                        "m",
                        latch_protocol::ReasoningEffort::ProviderDefault,
                    ))
                    .unwrap();
                let provider = registry
                    .build_provider(&profile, &descriptor, &credentials, Uuid::new_v4(), None)
                    .unwrap();
                let provider = if child {
                    provider.for_session(Uuid::new_v4()).unwrap()
                } else {
                    provider
                };
                let result = tokio::time::timeout(
                    Duration::from_secs(8),
                    provider.stream(request(), CancellationToken::new(), Arc::new(|_| {})),
                )
                .await
                .expect("outer watchdog");
                let error = result.unwrap_err();
                let timeout = error
                    .downcast_ref::<ProviderTimeout>()
                    .expect("typed connection timeout");
                assert_eq!(
                    timeout.phase,
                    TimeoutPhase::Connect,
                    "{transport}, child={child}: {error}"
                );
                assert_eq!(timeout.seconds, 1);
                assert!(error.to_string().contains("TLS handshake"));
                assert!(retry_delay(&error, 0, Uuid::nil()).is_some());
                tokio::time::timeout(Duration::from_secs(2), server)
                    .await
                    .expect("TLS connection released")
                    .unwrap();
            });
        }
    }
    futures::future::join_all(cases).await;
}

#[test]
fn defaults_and_config_validation_are_backward_compatible() {
    let config: crate::config::Config = toml::from_str("").unwrap();
    assert_eq!(config.provider_request, ProviderRequestConfig::default());
    assert_eq!(config.provider_request.connect_timeout_seconds, 15);
    assert_eq!(config.provider_request.headers_timeout_seconds, 60);
    let previous: crate::config::Config =
        toml::from_str("[provider_request]\nheaders_timeout_seconds = 120").unwrap();
    assert_eq!(previous.provider_request.connect_timeout_seconds, 15);
    assert_eq!(previous.provider_request.headers_timeout_seconds, 120);
    for key in [
        "connect_timeout_seconds",
        "headers_timeout_seconds",
        "idle_timeout_seconds",
        "request_timeout_seconds",
    ] {
        for seconds in [0, 86_401, u64::MAX] {
            let text = format!("[provider_request]\n{key} = {seconds}");
            // TOML integers above i64::MAX are rejected during parsing itself.
            if let Ok(config) = toml::from_str::<crate::config::Config>(&text) {
                assert!(config.validate().is_err());
                assert!(crate::providers::ProviderRegistry::from_config(&config).is_err());
            }
        }
    }
    let encoded = toml::to_string(&config).unwrap();
    let decoded: crate::config::Config = toml::from_str(&encoded).unwrap();
    assert_eq!(decoded.provider_request, config.provider_request);
}
