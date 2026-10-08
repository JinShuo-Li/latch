# Web UI implementation plan

Status: Linux integration implemented. The original agreed plan below records
the design and implementation hints; [WEB_UI.md](WEB_UI.md) documents the
current commands, controls, trust boundary and verification. The reviewed
prototype remains separate from the embedded production assets.

## Agreed scope

- Preserve the reviewed interface in [`web/prototype/`](../web/prototype/README.md):
  English, dark by default, a conversation-first layout, collapsible task
  details, and sans-serif workspace paths. Keep the sidebar slogan removed.
- Use the same kernel, durable sessions, configuration, providers, approvals,
  tools, and sandbox semantics as the TUI.
- Fix the workspace to the canonical directory where Latch starts. Do not
  introduce a project picker or accept a replacement workspace from the browser.
- Target a **Linux Web server**. Windows Web implementation and dedicated
  Windows qualification are outside this work. Preserve the existing Windows
  CLI/TUI build and runtime; do not change their workflows or sandbox behavior.
- Support local browsers and SSH local port forwarding. SSH mode is a launch
  mode for the same server, not an SSH client or a separate backend.
- Leave computer use for a later plugin. Establish reusable event and media
  transport without designing or implementing desktop control now.

## Agreed CLI contract

```sh
# Local use: loopback port 6006, open the browser and print the access URL.
latch --web

# Local use on a different port.
latch --web --web-port 6007

# Run on the remote Linux host: loopback port 6006, do not open a browser.
latch --web --ssh 6006

# Resume the latest session in the fixed startup workspace.
latch --web --resume --latest
```

Define `--ssh <REMOTE_WEB_PORT>` with help text explaining that the argument
is the remote Web listener port and SSH mode suppresses browser launch.
Require `--web` for `--ssh` and `--web-port`; make the two port options mutually
exclusive. Accept ports 1–65535. Reject mixing Web mode with machine subcommands
or `-p` rather than silently choosing a different execution path. Preserve
the existing config, provider, model, effort, mode, and attachment overrides.

If `--resume` has no selector, use the browser session picker without requiring
a terminal. An explicit session selector must belong to the fixed workspace;
do not silently switch directories as the legacy interactive path can do.
Bind a requested port or return an actionable error; do not silently select
another port. A browser-launch failure should leave the server running and
print instructions to open its URL manually.

On the local machine, forward the remote listener:

```sh
ssh -N -L 6006:127.0.0.1:6006 user@remote-host
```

If the local port is occupied, use a different local port:

```sh
ssh -N -L 7000:127.0.0.1:6006 user@remote-host
```

Open `http://localhost:7000` in the second example and authenticate with the
startup access token. The printed instructions must distinguish local and
remote ports. Do not put credentials in the SSH command. Use relative frontend
URLs throughout; a forwarded local port need not match the remote port.

## Architecture and ownership

```text
TUI -----------------------+
                           +--> shared session controller --> Agent / EventStore
Browser --> HTTP + SSE ----+
```

Keep orchestration in the CLI and truth in the kernel. Start with these sources:

| Existing source | Reuse or extraction |
| --- | --- |
| `crates/latch-cli/src/main.rs` | `interactive_session`, steering/cancellation, permission broker, live profile changes, setup application, slash-command dispatch |
| `crates/latch-cli/src/cli/session.rs` | Session construction, restore/replay, configuration context, image ingestion; separate terminal pickers from selection |
| `crates/latch-tui/src/lib.rs` | `Input` / `Output` and display catalogs currently owned by the TUI; separate common contracts from terminal state |
| `crates/latch-tui/src/presentation.rs` | Semantic cells and identical live/replay presentation |
| `crates/latch-tui/src/sidebar.rs` | Task, evidence, usage, ownership, agent and group projections; keep Ratatui rendering outside common state |
| `crates/latch-cli/src/cli/command.rs` | Flag parsing and conflict tests |
| `crates/latch-cli/tests/cli.rs` | Real-binary loopback-provider fixtures for integration tests |

Use a small shared UI crate if needed for contracts and semantic reducers that
both interfaces consume. Keep it free of HTTP and Ratatui dependencies. Do not
put UI configuration forms or transport DTOs into the durable protocol schema,
and do not move interface state into `Agent`. Preserve TUI imports through
re-exports where useful while migrating ownership.

Put the Web server adapter under `crates/latch-cli/src/web/`. A Tokio HTTP
framework such as Axum is appropriate; avoid hand-written HTTP parsing.
Retain the approved HTML/CSS and split prototype JavaScript into transport,
state, and view modules. A framework migration is not required to connect it.
Embed production assets in the binary. Python remains only a prototype preview
dependency; production Web use must require neither Python nor Node.

## API and event hints

Treat these routes as a proposed versioned interface, not an existing API:

| Operation | Suggested endpoint |
| --- | --- |
| Authenticate with the startup token | `POST /api/auth` |
| Get workspace, catalogs, active state, and capabilities | `GET /api/bootstrap` |
| List root sessions for the fixed workspace | `GET /api/sessions` |
| Create or select a session | `POST /api/sessions`, `POST /api/sessions/:id/activate` |
| Submit input, cancel, resolve approval, change profile/settings | `POST /api/sessions/:id/commands` |
| Subscribe to semantic state and text deltas | `GET /api/events` (SSE) |
| Ingest an image through the kernel | `POST /api/sessions/:id/attachments` |
| Fetch validated session media | `GET /api/sessions/:id/media/:artifact_id` |

Initially allow one active root-session controller per server. Child sessions
remain managed by the existing supervisor. Reject switching root sessions while
a turn is active; let the user explicitly cancel first. Switching should replace
the session controller without restarting the listener or changing its URL.

Commands should carry an expected session ID and a unique command ID. Prevent
duplicate submissions on retries. An HTTP acknowledgement means accepted, not
that a task or durable state change has succeeded. Publish committed changes
only after the existing kernel operation succeeds. Return structured errors
and expose them in the interface rather than leaving it stuck in a busy state.

Use semantic snapshots plus ordered events; do not serialize the TUI `App`,
`Agent`, SQLite rows, provider wire requests, or secrets. Include a server
instance ID, session ID, and stream sequence in the transport. Subscribe before
taking a snapshot and reconcile buffered events so no event is lost between
bootstrap and streaming. Bound subscriber queues and replay buffers. A slow
client must not block execution; require a new snapshot if its cursor expires.

The durable log remains authoritative. Transport buffers are disposable and
must never become a second history database. Keep in-progress text separately
from completed messages so reconnecting cannot duplicate an assistant response.
Do not append browser notifications or every text delta to durable history.

Browser disconnects and SSH tunnel interruptions must not automatically cancel
a turn or resolve an approval. On reconnect, recover the active snapshot and
pending request. On server Ctrl+C, use the existing cancellation and process /
extension cleanup paths. Preserve steering's end-of-turn race handling.

## Authentication, attachments, and remote use

Bind only `127.0.0.1`, including in SSH mode. Generate a random startup access
token. For automatic local opening, place it in the URL fragment, exchange it
for an HttpOnly same-site session cookie, and remove the fragment from the
address bar. SSH users can enter the token at the forwarded address. Never
log tokens, request bodies containing secrets, or provider credential values.

Validate Host and same-origin mutations without assuming the browser's port
equals the listener's port. Do not enable broad CORS. SSE must use the same
authenticated session as command endpoints. Validate approval IDs against the
active session and pending broker request; stale, duplicate, or unknown
decisions must not authorize execution. Render model/tool text as text or
sanitized Markdown; never insert untrusted HTML.

Do not expose arbitrary filesystem paths or serve the workspace directory.
Enforce upload limits before buffering image bodies, then use the existing
kernel image validation and immutable artifact ingestion. The browser uploads
its local file bytes; it cannot supply a path on the server. Associate resulting
`MediaRef`s with the intended session and check access when serving previews.

Under SSH, workspaces, tools, model calls, configuration, and credentials live
on the remote server. The browser provides presentation and human input.
Future computer-use screenshots and actions can use the same authenticated
transport; desktop permissions and plugin lifecycle still belong to the kernel.

## Delivery sequence and acceptance

1. Extract the common interactive controller and contracts. Verify unchanged
   TUI behavior, restore, steering, permissions, and provider serialization.
2. Add Linux Web launch, embedded assets, authentication, and SSH instructions.
   Verify flag conflicts, port errors, fixed workspace, and headless startup.
3. Connect session selection, streaming, additional instructions, cancellation,
   approvals, and reconnect. Remove fixtures from these connected flows.
4. Connect configuration, provider/model discovery, mode/safety/permissions,
   attachments, tool/raw output, diffs, task/evidence/usage, agents and groups.
   Maintain a TUI parity checklist; do not declare completion while a control
   still presents mock success or an example result.
5. Validate locally, update ownership/architecture documentation, and finish
   integration with isolated deterministic tests and a clean release gate.

Make each concern an atomic Conventional Commit. Split a large milestone into
smaller working commits when needed. Stage only intended files, push to main,
and watch the existing GitHub Actions runs. No new Windows-specific Web tests
or manual Windows runtime qualification are required for this Linux scope.

Linux acceptance includes real-binary tests against a loopback mock provider,
refresh during streaming, approval reconnect, stale decisions, command retries,
cancellation/steering races, image validation, session/workspace boundaries,
and expired stream cursors. Check SSH forwarding with equal and unequal local /
remote ports and tunnel loss/recovery. Never use paid live providers by default.

Run the Linux release gate for significant implementation changes:

```sh
cargo fmt --all -- --check
bash scripts/release-gate.sh
```

Use relevant continuity/long-session tests if shared event or context behavior
changes. Test desktop/mobile layouts and core flows in a real browser. Update
`docs/ARCHITECTURE.md`, affected runtime/continuity documentation, README, and
AGENTS.md when implementation changes their contracts or ownership. This document records the agreed plan; the Web guide describes implemented
runtime behavior.
