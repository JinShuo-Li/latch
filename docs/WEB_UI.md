# Web UI (Linux and Windows)

The browser uses the same interactive controller and kernel as the TUI. It
starts in English with the approved dark layout. The workspace is fixed to the
canonical directory where `latch` starts; selecting a conversation never changes
that directory. Web support is available in source builds from current main;
published v0.2.3 binaries predate this integration.

## Start locally

```sh
cd /path/to/project
latch --web
# Choose another port if 6006 is occupied:
latch --web --web-port 6007
```

The server binds only `127.0.0.1`. It prints an access URL and opens the default browser with `xdg-open` on Linux
or the Windows URL handler on Windows.
If no desktop browser is available, open the printed URL manually. Assets are
embedded in the binary: Python, Node, npm, and a separate frontend service are
not runtime requirements. Linux requires `bwrap`; Windows uses the embedded AppContainer runner. Both
require Git and ripgrep for the corresponding tools. On Windows, use a
user-owned NTFS checkout whose objects permit temporary ACL grants; starting
the browser does not prove the workspace is executable.

Config/provider/model/effort/mode/attachment overrides remain available. Resume
with `latch --web --resume --latest` or `--resume --session <ID>`. A bare
`--resume` opens the browser session picker without a terminal prompt. Only
sessions in the launch workspace can be selected. CLI `--attach` images are
retained through the browser picker and applied once to the first selected or
created session. Stop the server with Ctrl+C
in the launching terminal, or Settings → General → Stop Web server.

## First-run setup

Startup resolves the configuration on the Latch host: `--config <PATH>` when
specified, otherwise `~/.latch/config.toml` (an existing legacy XDG installation
continues to be read in place). A default interactive launch without that file
opens setup even if an OpenAI key is present in the environment. Legacy OpenAI
fallback values are not shown as a configured browser account. Missing
credentials also open provider settings. Replacing the active provider’s missing
credential unlocks the current session immediately when it resolves. Explicit provider/model/effort launch
overrides retain their existing behavior.

The setup dialog shows the actual configuration destination. Choose a provider,
enter an API key or reference a host environment variable, then choose a default
model, enabled models and reasoning effort from the shared catalog. Custom
endpoints expose base URL, model ID and transport. API keys are stored privately
on the host; they are never returned to the browser. Saving uses the same
controller and validation as the TUI. On subsequent starts, Latch loads the saved
configuration automatically.

Save feedback stays in the dialog. A rejected save retains the form for retry;
a successful save clears the key and displays the updated provider. Availability
refreshes update the current model list automatically. Unresolved models open
the transport editor. Existing provider/model overrides, credentials, defaults,
pricing and effort mappings remain editable; provider removal requires an
explicit confirmation.

## Activity

The left sidebar shows request preparation, waiting for model activity,
provider-reported reasoning, response text, tool preparation/execution, pending
approval, cancellation and the final turn outcome. It also shows elapsed phase
time and time since the latest activity. Timers update while no text arrives.
After 30 seconds without activity it offers a Stop reminder, without declaring
that the model failed or is thinking. Reasoning text is never sent to this panel.
A resumed unfinished turn is marked interrupted; replay does not restart it.

The TUI uses the same activity reducer in its sidebar and refreshes once per
second during silence. The browser also receives a five-second connection
heartbeat. A connection heartbeat does not reset model/tool activity timers or
prove model progress. A finished turn is distinct from verified task completion,
which remains in Overview. Closing the browser or losing the tunnel does not
cancel execution.

## Chat tables

Assistant replies, live streamed text and agent reports render Markdown tables
with header cells and left/center/right column alignment. Escaped pipes and
pipes inside inline code stay inside their cells. Wide tables scroll horizontally
inside the transcript instead of widening the page; the scroll region supports
keyboard focus. Markdown-looking tables inside fenced code remain code. All
cell text is escaped before formatting.

## SSH forwarding

On the remote Linux or Windows host (with an SSH server):

```sh
cd /path/to/project
latch --web --ssh 6006
```

`--ssh <REMOTE_WEB_PORT>` selects the **remote listener port** and suppresses
browser launch. Latch prints tunnel instructions; it does not start an SSH
client. On your local machine:

```sh
ssh -N -L 6006:127.0.0.1:6006 user@remote-host
# A different local port is also supported:
ssh -N -L 7000:127.0.0.1:6006 user@remote-host
```

Open `http://localhost:7000` for the second example and enter the token printed
by the remote Latch process. Alternatively, append `/#token=<TOKEN>` to that
local address. All API/media/stream URLs are relative to the browser address.
Do not put the token in the SSH command. `--ssh` and `--web-port` require
`--web`, accept ports 1–65535, and cannot be used together. An occupied port is
an error, not an automatic port change.

Tools, configuration, credentials and model requests execute on the remote
host. Image uploads originate in the local browser and are ingested into the
active remote session's artifact store. Losing the tunnel or closing the page
leaves an active turn running; reopening the page recovers its current state.

## Everyday controls

- Enter sends; Shift+Enter inserts a line break. Send additional instructions
  while a turn runs, or use Stop to cancel. Up/Down at the composer boundary
  recalls prompt history. Ctrl/⌘+K starts a new conversation when idle.
- Select Ask/Plan/Work, a configured model and its supported reasoning effort.
  Missing initial configuration opens provider settings. Settings exposes
  providers, credentials, model overrides, safety and approval
  resolution. Configuration results appear in the settings dialog and conversation;
  HTTP acceptance alone is not reported as a successful save.
- Review pending approvals including the tool, complete arguments, reason and
  capabilities. Allow once/Deny resolve the kernel's pending request. A decision
  for an already resolved request is rejected.
- Tool output can be expanded, copied or inspected raw. Task details show
  kernel task/evidence/usage records, change ownership, child agents and groups.
  Workspace diff, checkpoint, undo, context inspection and context reset use
  the existing kernel operations. `/help` opens the shared command catalog.
- Attach PNG/JPEG/WebP files through the button, drag/drop, or `/attach` without
  a path. Browser attachments do not accept server filesystem paths. The kernel
  validates format, structure, dimensions and the 5 MiB limit. Pending images
  survive browser refresh while the server runs; sent images are durable session
  media. A detached draft does not delete an already referenced artifact.

New/resume and profile/settings changes wait until the active turn finishes or
is cancelled. Every connected tab controls the same active session. A stale tab
must reload after another tab switches sessions. The server restart invalidates
its authentication; use the new printed token. Unsent drafts, browser appearance,
transient stream deltas and UI notices are not durable session history.

## Transport and trust

The Linux/Windows adapter is `crates/latch-cli/src/web/`: `mod.rs` owns startup and
host lifetime, `actor.rs` serializes actions/session ownership, `http.rs` owns
routes/authentication/SSE, and `state.rs` maintains a disposable projection.
The host reuses one database connection for session listing and selection;
reading the session list does not reopen the database or rebuild group
projections alongside an active writer.
`web/app/` contains the production assets; `web/prototype/` remains a separate
fixture-based design example. `latch-ui` owns shared contracts, catalogs and
semantic reducers; `cli/interactive.rs` owns the shared session controller.

The public page serves an explicit embedded asset allowlist, never the workspace.
API access requires an HttpOnly, SameSite=Strict cookie obtained by exchanging a
random startup token. The token is removed from the URL fragment immediately.
Host and Origin checks allow only the same loopback browser origin, including a
different forwarded local port. There is no CORS or public listener option.
Responses disable caching and apply a content security policy. Assistant/tool
text is escaped before rendering; model output cannot inject HTML or scripts.
Credentials are written through the existing configuration flow and are never
returned by bootstrap/catalog responses.

Browser input is a typed `latch-ui::Input` request, scoped to an active session.
A command carries an instance ID, UUID and server-issued timestamp. Replays with
the same UUID/payload return the existing acceptance result; changed payloads,
wrong instances and requests older than five minutes are rejected. The bounded
command receipt cache refuses new commands if full, rather than forgetting a
still-valid receipt. These receipts coordinate transport, not durable completion.
The kernel still owns policy, validation, permissions, execution and persistence.

SSE sends an initial authoritative snapshot, small sequence-change signals,
and connection heartbeats.
The browser coalesces signals and fetches current state. A bounded broadcast
ring resnapshots lagging observers; reconnect always starts with a full snapshot,
regardless of Last-Event-ID. Sequence IDs are per server instance and are never
SQLite event cursors. Durable transcript/sidebar state is reduced from kernel
events on resume; transient assistant deltas belong only to the live server.
Disconnecting an observer does not disconnect the controller. Streaming output
is relayed in order so a full output channel cannot silently lose durable events.

Computer use remains a future plugin. This transport already carries validated
session images and real permission requests; it grants no desktop-control
capability and does not change the extension protocol.

## Verification

```sh
cargo test -p latch-cli --test cli web_transport --locked
cargo test -p latch-ui -p latch-tui --locked
bash scripts/release-gate.sh
```

The Linux real-binary tests use an isolated loopback mock provider and state
store, covering authentication/origin/Host checks, forwarded authority,
embedded assets, command retries, durable resume, configuration errors,
approvals, busy session switches, image validation/media boundaries,
steering/reconnect, cancellation, shutdown, default-path setup/restart and argument
conflicts. No live provider credential is needed. Native Windows qualification
and its remaining gaps are tracked in [Windows diagnostics](WINDOWS_DIAGNOSTICS.md).

Optional frontend regression checks use production assets with an isolated
transport fixture in Chromium (Node 22+; no npm packages or provider calls):

```sh
node scripts/test_web_markdown.mjs
node scripts/test_web_setup.mjs /path/to/chromium
```

This checks first-run and configured startup, credential modes, rejected/pending
saves, model selection, live refresh, preserved drafts, custom models and mobile
layout, plus chat table alignment, escaping, streaming and scrolling. The
Markdown parser checks need only Node; browser layout checks need Chromium.
Both are development-only dependencies.

## TUI control parity

| TUI function | Connected browser control |
| --- | --- |
| Submit, steer, cancel | Composer, additional instructions, Stop |
| Resume and prompt history | Workspace-scoped conversation list, Up/Down |
| Mode, model, reasoning effort | Composer mode selector, model/effort dialog |
| Provider setup and model discovery | Settings → Providers, Refresh availability, advanced model edits |
| Safety and permission resolver | Settings → Safety |
| Pending human permission | Complete request details, Allow once/Deny |
| Attach/list/detach images | Attachment button/drop, draft chips, `/attachments`, `/detach` |
| Semantic/raw tool output | Expandable tool cards, raw inspection, copy |
| `/diff`, `/checkpoint`, `/undo` | Changes panel and slash commands |
| `/context`, `/compact` | Overview actions and slash commands |
| Canonical task, evidence, usage/cost | Overview and expandable recorded state |
| Child agents, `/group` | Agents panel and shared group overview |
| `/help`, `/raw`, `/sidebar`, `/quit` | Command catalog, raw transcript toggle, details toggle, server shutdown |

The browser implements its own focus, layout and shortcuts. The shared catalog,
controller and semantic reducers determine actions and displayed kernel records;
there are no fixture-driven success paths in production assets.
