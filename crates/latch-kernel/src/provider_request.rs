//! Provider-neutral request deadlines and conservative retry classification.
//! No wire serialization or durable conversation state lives here.

use std::{fmt, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use async_trait::async_trait;
use latch_protocol::{ModelRequest, ModelResponse, StreamActivity, StreamEvent};
use serde::{Deserialize, Serialize};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::provider::{ModelProvider, StreamSink};

#[cfg(test)]
mod tests;

/// Model network deadlines, independent of command execution timeouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderRequestConfig {
    pub headers_timeout_seconds: u64,
    pub idle_timeout_seconds: u64,
    pub request_timeout_seconds: u64,
}

impl Default for ProviderRequestConfig {
    fn default() -> Self {
        Self {
            headers_timeout_seconds: 120,
            idle_timeout_seconds: 180,
            request_timeout_seconds: 1800,
        }
    }
}

impl ProviderRequestConfig {
    pub fn validate(self) -> Result<()> {
        for (name, seconds) in [
            ("headers_timeout_seconds", self.headers_timeout_seconds),
            ("idle_timeout_seconds", self.idle_timeout_seconds),
            ("request_timeout_seconds", self.request_timeout_seconds),
        ] {
            // Bound Instant arithmetic as well as rejecting accidental disabling.
            if !(1..=86_400).contains(&seconds) {
                bail!("provider_request.{name} must be between 1 and 86400 seconds");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    Headers,
    Idle,
    Request,
}

#[derive(Debug)]
pub struct ProviderTimeout {
    pub phase: TimeoutPhase,
    pub seconds: u64,
}

impl fmt::Display for ProviderTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let phase = match self.phase {
            TimeoutPhase::Headers => "waiting for response headers",
            TimeoutPhase::Idle => "waiting for stream activity",
            TimeoutPhase::Request => "reaching the total request deadline",
        };
        write!(
            f,
            "model request timed out after {}s: {phase}; check the provider/network or adjust [provider_request]",
            self.seconds
        )
    }
}
impl std::error::Error for ProviderTimeout {}

/// Sanitized HTTP diagnostics retain machine-readable retry facts.
#[derive(Debug)]
pub(crate) struct ProviderHttpError {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub message: String,
}
impl fmt::Display for ProviderHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ProviderHttpError {}

/// Unknown failures are not evidence that resubmitting is safe. The caller
/// additionally refuses retries after any semantic output has begun.
pub(crate) fn retry_delay(error: &anyhow::Error, attempt: u32, session: Uuid) -> Option<Duration> {
    let retry_after = if let Some(http) = error.downcast_ref::<ProviderHttpError>() {
        if !matches!(http.status, 408 | 429 | 500 | 502 | 503 | 504) {
            return None;
        }
        http.retry_after
    } else if let Some(timeout) = error.downcast_ref::<ProviderTimeout>() {
        if timeout.phase == TimeoutPhase::Request {
            return None;
        }
        None
    } else {
        let transport = error.downcast_ref::<reqwest::Error>()?;
        if !(transport.is_connect() || transport.is_timeout() || transport.is_body()) {
            return None;
        }
        None
    };
    // Bounded exponential backoff with stable per-session jitter. Respect
    // Retry-After seconds without permitting an unbounded server-directed wait.
    let backoff =
        Duration::from_millis(1000 * (1u64 << attempt.min(5)) + (session.as_u128() % 251) as u64);
    Some(
        backoff
            .max(retry_after.unwrap_or_default())
            .min(Duration::from_secs(60)),
    )
}

pub(crate) struct BoundedProvider {
    inner: Arc<dyn ModelProvider>,
    policy: ProviderRequestConfig,
}
impl BoundedProvider {
    pub fn wrap(
        inner: Arc<dyn ModelProvider>,
        policy: ProviderRequestConfig,
    ) -> Arc<dyn ModelProvider> {
        Arc::new(Self { inner, policy })
    }
}

#[derive(Clone, Copy)]
struct Progress {
    connected: bool,
    last: Instant,
}

#[async_trait]
impl ModelProvider for BoundedProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn model(&self) -> &str {
        self.inner.model()
    }
    fn for_session(&self, session_id: Uuid) -> Option<Arc<dyn ModelProvider>> {
        self.inner
            .for_session(session_id)
            .map(|inner| Self::wrap(inner, self.policy))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let start = Instant::now();
        let (progress_tx, mut progress_rx) = watch::channel(Progress {
            connected: false,
            last: start,
        });
        let observed: StreamSink = Arc::new(move |event| {
            let active = match &event {
                StreamEvent::Activity(_)
                | StreamEvent::ToolCallDelta(_)
                | StreamEvent::Usage(_) => true,
                StreamEvent::TextDelta(text) => !text.is_empty(),
                _ => false,
            };
            if active {
                progress_tx.send_modify(|progress| {
                    // Only adapters' Connected signal marks receipt of headers.
                    progress.connected |=
                        matches!(event, StreamEvent::Activity(StreamActivity::Connected));
                    progress.last = Instant::now();
                });
            }
            sink(event);
        });
        let future = self.inner.stream(request, cancel.clone(), observed);
        tokio::pin!(future);
        let total = start + Duration::from_secs(self.policy.request_timeout_seconds);
        let mut observing = true;
        loop {
            let progress = *progress_rx.borrow_and_update();
            let (phase, seconds, deadline) = if progress.connected {
                let seconds = self.policy.idle_timeout_seconds;
                (
                    TimeoutPhase::Idle,
                    seconds,
                    progress.last + Duration::from_secs(seconds),
                )
            } else {
                let seconds = self.policy.headers_timeout_seconds;
                (
                    TimeoutPhase::Headers,
                    seconds,
                    start + Duration::from_secs(seconds),
                )
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => bail!("model request cancelled"),
                () = tokio::time::sleep_until(total) => return Err(ProviderTimeout { phase: TimeoutPhase::Request, seconds: self.policy.request_timeout_seconds }.into()),
                result = &mut future => return result,
                changed = progress_rx.changed(), if observing => {
                    // A custom adapter may drop its sink before finishing.
                    observing = changed.is_ok();
                }
                () = tokio::time::sleep_until(deadline) => return Err(ProviderTimeout { phase, seconds }.into()),
            }
        }
    }
}
