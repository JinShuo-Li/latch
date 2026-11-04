# Model request deadlines and retries

Model network waits are bounded independently of shell commands, validation,
MCP and extension execution. These limits apply to all model adapters built by
`ProviderRegistry`, including child sessions and stateless permission reviews.
TUI, Web and machine CLI use the same kernel policy.

## Defaults

```toml
[provider_request]
connect_timeout_seconds = 15
headers_timeout_seconds = 60
idle_timeout_seconds = 180
request_timeout_seconds = 1800
```

| Limit | Meaning |
| --- | --- |
| Connect | Maximum time to establish a new connection, including DNS/TCP, proxy negotiation and TLS. Reusing an existing connection does not start a new connection budget. |
| Headers | Maximum wait from starting an adapter request until it signals a successful HTTP response, including credential access, request preparation, connection/TLS, upload and response-header processing. Error-body reads are covered too. |
| Idle | Maximum silence after connection. Any received stream bytes (including SSE keepalives), reasoning activity, text, tool fragments or usage reset this deadline. |
| Request | Absolute deadline per adapter attempt, including the header and streaming phases. Heartbeats cannot extend it. |

Every value must be an integer from 1 to 86,400 seconds. Zero does not disable a
limit. The connection budget is contained within the header budget, not added
to it: with defaults, a failed connection stops at about 15 seconds, while a
connected server has at most 60 seconds from the start of the attempt to return
headers. A shorter header or total deadline still wins. These limits do not
impose a 15-second lifetime on an established stream. Existing configurations
get the new defaults for omitted fields without migration; explicit values
(such as an existing 120-second header override) remain unchanged. Increase the limits for a
provider with unusually long silent reasoning periods; silence does not prove
that a model has stopped thinking. In particular, 180 seconds is **not a thinking
time limit**: reasoning bytes or SSE heartbeats renew idle, so thinking can
continue for more than 180 seconds. Only uninterrupted silence expires that
deadline. On expiry the partial response is not treated as success; it is dropped
locally. Before text/tool output, the normal bounded retry policy still applies.
These are local resource limits, not claims about provider-side completion.

Cancellation interrupts request waits and retry backoff. Expiry drops the
adapter future and its HTTP stream; the kernel does not leave a detached
network reader running. Diagnostics distinguish connection/TLS timeout, waiting
for headers, waiting for stream activity, and the absolute deadline. They do not include credentials.

## Conservative automatic retries

The agent permits at most two automatic retries, and only before semantic
output begins. Retryable failures are:

- Connection/TLS, header or stream-idle timeout (not the absolute request deadline).
- Recognized connection, transport timeout or response-body I/O errors.
- HTTP 408, 429, 500, 502, 503 and 504.

Authentication, permission, invalid-request and unknown errors fail directly.
ChatGPT subscription usage-limit diagnostics remain non-retryable: Latch does
not switch to API-key billing or reinterpret account limits as temporary
network failures.

Backoff starts at one second, then two seconds, plus bounded per-session jitter.
`Retry-After` seconds and HTTP dates are honored up to a maximum wait of 60
seconds. Longer server delays are deliberately capped. Each scheduled retry
commits a durable `FailureAttempt` before waiting; cancellation during the wait
prevents another submission. This network retry ceiling is separate from
`failure.retry_budget`, which governs tool/validation supervision.

After nonempty assistant text, a tool-call fragment/activity signal or a
completion event, failures are **not automatically replayed**. The error retains
the original cause and explains that a retry must be initiated manually. Failed
partial tool calls are never executed; only a successfully returned model
response can enter durable tool dispatch. Transient fragments are not promoted
to durable conversation truth.

A timeout cannot prove that the remote server abandoned an accepted request;
a retry can incur additional provider usage even before output arrives. This
policy bounds local waiting and avoids replaying visible partial output. It
does not promise provider-side exactly-once execution or billing.

## Ownership and checks

- `provider_request.rs`: policy configuration, provider-neutral deadlines,
  typed timeout facts and retry classification.
- `providers.rs`: applies the policy when constructing adapters; child rebinding
  preserves both policy and adapter-specific session identity.
- `provider.rs`: HTTP client connection limits, provider wire formats and
  sanitized HTTP status/Retry-After extraction. Subscription clients keep their
  no-redirect policy when connection limits are configured. OpenCode Go endpoint, model selection and stable session header
  are unchanged.
- `agent.rs`: output-aware retry admission, durable attempt recording and
  cancellable backoff. No failed response dispatches tools.

Focused, credential-free checks:

```sh
cargo test -p latch-kernel --lib provider_request --locked
cargo test -p latch-kernel --lib model_retr --locked
cargo test -p latch-kernel --lib retry_after --locked
```

Paused-clock fixtures cover phase deadlines, heartbeat renewal, absolute
limits, cancellation and rebinding. Real loopback HTTP fixtures stall both
before headers and after connection across Chat Completions, Responses,
Anthropic and Gemini, and verify that timeout releases the TCP connection.
Additional registry-built root/child fixtures accept TCP but never finish TLS,
proving the configured connection deadline expires before the header deadline.
Paused-clock reasoning fixtures run past 180 seconds successfully with activity,
while a truly silent stream expires at 180 seconds. Subscription HTTP clients
are checked to keep redirects disabled in either configuration order.
