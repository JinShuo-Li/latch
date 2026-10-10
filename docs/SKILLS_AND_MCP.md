# Agent Skills and MCP

Latch v0.3.2 supports the [Agent Skills specification](https://agentskills.io/specification)
and MCP tool servers over sandboxed stdio and Streamable HTTP. Neither integration
can change kernel state, grant permissions, certify evidence, or bypass the sandbox.
The existing Latch extension protocol and SDKs are unchanged.

## Agent Skills

Discovery checks these directories, in this order:

1. `<workspace>/.latch/skills`
2. `<workspace>/.agents/skills`
3. `<workspace>/.claude/skills`
4. The same three paths under the user's home directory.

Each immediate child directory must contain `SKILL.md`. The first valid skill of
a given name wins. Directory names must match the frontmatter `name`. Discovery
is deterministic and limited to 512 skills; invalid skills produce diagnostics
without preventing other skills from loading. Use `latch skills` or `/skills` to
inspect the catalog. Restart the session to discover new skills.

```text
.latch/skills/review/
  SKILL.md
  references/checklist.md
  scripts/check.py
  assets/template.txt
```

```markdown
---
name: review
description: Review Rust changes against the repository's conventions.
compatibility: Requires cargo and ripgrep.
metadata:
  author: example
---
Read references/checklist.md before reviewing changes.
Use ordinary command tools to run scripts/check.py when appropriate.
```

YAML frontmatter supports `name`, `description`, `license`, `compatibility`,
`metadata`, and experimental `allowed-tools`. The latter is informational and
**never** pre-approves tools. Only metadata goes into the session prompt. The
model calls `load_skill` with `{"name":"review"}` to load instructions, then
`{"name":"review","path":"references/checklist.md"}` for resources. Text reads
are limited to 1 MiB and must stay within the skill's real root; traversal,
escaping symlinks, hardlink aliases, and non-UTF-8 resources are rejected.

Scripts are not executed by the loader. Workspace scripts run through the
normal sandboxed tools and approval rules. User-level scripts remain subject to
home masking; the model can read their text and explicitly create a workspace
copy before proposing execution. Binary assets use existing image/file tooling
where its permissions allow access, rather than being injected into context.
Skills are instructions, not trusted kernel policy.

The catalog is session context, never the stable provider system field. Loaded
instructions and resources are ordinary durable tool results: history, replay,
rotation, recall, and context budgets treat them like any other tool output.
Resume does not re-read a resource to reconstruct an old result. Root and child
agents discover skills independently in their shared workspace.

## MCP configuration

Add operator-owned entries to the normal Latch configuration file. Project files
and skills cannot silently configure or launch servers.

```toml
[[mcp_servers]]
name = "local-tools"
transport = "stdio"
command = "python3" # use "python" on Windows if that is the installed alias
args = ["/absolute/path/to/server.py"]
enabled = true
timeout_seconds = 120

[[mcp_servers]]
name = "remote-tools"
transport = "streamable_http"
url = "https://example.com/mcp"
bearer_token_env = "EXAMPLE_MCP_TOKEN"
enabled = false
```

`name` is a unique 1–32 character ASCII identifier (letters, digits, hyphens).
Deadlines are 1–3600 seconds. HTTP requires HTTPS except for literal loopback
and localhost. URLs containing user information or fragments are rejected.
Redirects and ambient proxy discovery are disabled. Bearer credentials are
resolved from the explicitly named environment variable, not written to config
or passed to unrelated servers. Stdio inherits the execution backend's minimal
sandbox environment; host credentials are not implicitly forwarded.

`latch mcp` lists configuration without contacting a server. `latch mcp --check`
connects enabled servers, discovers tools, reports the negotiated version, and
closes connections without making model or tool calls. Ctrl+C cancels startup.
The TUI's `/mcp` shows live status; `/mcp stop` disconnects the current session's
servers. Edit `enabled` in config and start a new session to enable/disable a
server. Schema registrations remain fixed for the session even after disconnect.

Every enabled server must initialize successfully before a session starts.
Child agents open independent clients using the root's operator configuration;
their calls, approvals, results and evidence belong to the child session.
Closing workers and sessions tears down their clients. No process or permission
grant is shared between sessions.

## Protocol and lifecycle

The client probes `server/discover` for modern MCP **2026-07-28**, sends the
required per-request protocol/client metadata, and supports HTTP method/name
headers and validated `x-mcp-header` parameter mirroring. A recognized modern
error never triggers a legacy initialization fallback. For legacy peers it
negotiates `initialize` / `notifications/initialized`, accepting **2025-11-25**,
**2025-06-18**, **2025-03-26**, and **2024-11-05**. Legacy Streamable HTTP sessions
carry `Mcp-Session-Id` and are terminated with DELETE. The deprecated separate
HTTP+SSE endpoint transport is not supported; configure a Streamable HTTP
endpoint instead. Protocol compatibility does not imply every optional MCP
feature is implemented.

Stdio uses newline-delimited JSON-RPC, independently of the Latch extension
protocol's Content-Length frames. HTTP accepts both JSON and request-scoped SSE,
including split frames, CRLF and keep-alives. Tool discovery supports pagination
and rejects repeated cursors and duplicate tools. Each server is limited to 512
tools and each request/response to 4 MiB. Provider-facing names use the server id
and a stable SHA-256 name suffix to avoid collisions and unsupported characters.

Startup, probes, calls, cancellation and cleanup have deadlines. Cancellation
sends a bounded stdio cancellation notification or closes the HTTP response
stream; legacy HTTP also receives a cancellation notification. A failed exchange
disconnects the client. There are no automatic tool retries: a timeout or lost
response does not prove that an external action failed to happen. Start a new
session to reconnect. Stdio closes stdin, waits briefly, then kills and reaps a
server that does not exit. Process lifetime remains owned by the platform
execution backend (Bubblewrap on Linux; native AppContainer/Job Object on Windows).

Only tool discovery/execution is exposed. Sampling, elicitation, client roots,
subscriptions, MCP resources/prompts, OAuth flows, and asynchronous tasks are not
advertised. Unsupported server interactions fail explicitly. Tools may return
structured/text content and `isError`; media remains serialized external data
rather than becoming an unvalidated image handle.

## Permissions and evidence

Stdio servers run with the existing extension sandbox: read-only workspace,
network access, masked home and protected state. There is no unsandboxed fallback.
HTTP servers execute remotely; Latch cannot sandbox a remote service. Their
invocations still pass kernel permissions and cancellation.

All MCP tools require WORK mode. Strict and Standard safety require a real
approval resolved through the configured permission resolver; Autonomous permits
execution. AI review falls back to human approval for MCP even when arguments
contain a `command` field; command-only review cannot certify an external tool.
Server `readOnlyHint`, `destructiveHint`, other annotations, skill
instructions and tool descriptions never authorize execution. Extension guards
can further restrict MCP calls. Before an MCP call, the kernel durably records
`WorkspaceMutationPossible` and makes old validation evidence stale, including
for failed/cancelled calls. External output cannot create Passed evidence or
complete the root task. Child reports remain child evidence.

The stable provider system prompt and provider adapters are unchanged. Catalog
schemas are fixed for one session; tool results use the normal append-only durable
history and Context Engine. Changes to operator configuration take effect on
new sessions/resume startup, as with existing extension configuration.

## Validation

```sh
cargo test -p latch-kernel --lib skills --locked
cargo test -p latch-kernel --lib mcp --locked
bash scripts/release-gate.sh
```

MCP tests use a credential-free loopback HTTP/SSE fixture and a real sandboxed
Python stdio server in both modern and legacy modes. They cover cancellation,
invalid configuration, header injection/collision, unsupported interactions,
conservative mode enforcement, evidence invalidation and stable schemas after
shutdown. Skill tests cover YAML validation, discovery precedence, progressive
disclosure, resource containment and durable tool results. Native Windows runs
must be serial (`-- --test-threads=1`) and require Python for the stdio fixture;
Linux fixture execution requires working Bubblewrap. Never skip failed sandbox
coverage or report Linux results as Windows qualification.

Specification references: [MCP versioning](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning),
[stdio](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio),
[Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http).
