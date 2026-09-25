# Latch

[Website and install guide](https://jinshuo-li.github.io/latch/)

Latch is a quiet, programmable terminal coding agent built around explicit state,
evidence, controlled execution, and continuous long-session memory. V0.2.2 is a
Linux-first Rust implementation with a native streamed tool loop, durable SQLite
sessions, OpenAI-compatible and Anthropic providers, kernel-owned validation
evidence, guarded coding tools, three orthogonal Mode/Safety/Permissions
controls behind a mandatory Bubblewrap sandbox, a modern Ratatui interface with
a slash command palette and real input editing, and language-independent process
extensions. The main agent can delegate bounded work to durable asynchronous
child Latch sessions through a fixed generic tool surface.

Latch is independent software. Pi and OpenAI Codex were studied as public
references for agent and terminal interaction behavior; Latch is not a fork and
has no runtime dependency on either project. Exact shallow reference revisions
are recorded under `references/`.

## Build and run

Stable Rust and a C toolchain are required.

```sh
cargo build --release
export OPENAI_API_KEY=...
./target/release/latch
# Or install the executable:
cargo install --path crates/latch-cli
```

Run `/setup` in the TUI to configure a known provider: choose the provider,
credential source (environment variable or a securely entered key stored
`0600`), models to enable, and its default model, then Save. Known endpoints
and model capabilities come from Latch's catalog. `/model` switches the live inference profile
(provider, model, effort) without restarting the session.
OpenCode Zen is available as `opencode-zen`; its built-in rows currently cover
the documented Responses, Messages, and Chat Completions endpoints. Gemini
rows require a separate transport and are not offered yet.
Custom providers ask for an endpoint and a model request id and display name.
Their model transport is set in Advanced before the model can run. Unknown
models with no transport remain visible in setup but are excluded from `/model`.

Configuration is provider-neutral and multi-provider:

```toml
[providers.opencode-go]
kind = "opencode-go"
credential = "env:OPENCODE_API_KEY"
default_model = "deepseek-v4-flash"

[inference]
provider = "opencode-go"
model = "deepseek-v4-flash"
effort = "low"
```

`enabled_models` may select a subset of a provider's catalog; when absent,
the full built-in catalog plus custom entries is offered. Per-model tables
contain only fields the user overrides. Latch does not write built-in model
facts into the config.
Each provider's `default_model` is used when switching to that provider.
`[inference]` seeds new sessions and is set by the first usable setup; later
provider saves preserve it. A running session's `/model` choice is separate.

Credentials are symbolic (`env:NAME`, `file:NAME`, or `keyring:NAME`) and are
never stored in the config, the durable event log, the transcript, or logs.
Setup stages and syncs config and secrets before Save. A stored key is committed
first, so an interrupted save cannot leave config pointing at an uncommitted
secret; a failed config commit may leave an unused secret for later cleanup.
The legacy single `[provider]` table (and global `[models.*]` metadata) still
loads and migrates automatically, so existing configs keep working. Provider
requests identify as `latch/0.2.2`; OpenCode Go endpoints additionally receive
a stable `x-opencode-session` header carrying the durable session id, so
`--resume` keeps the same value. Model metadata precedence is explicit user
configuration > built-in catalog > conservative default; unknown models never
receive invented context windows, pricing, cache semantics, or reasoning
parameters, and unsupported effort values are never sent on the wire.

Per-invocation overrides: `latch --provider opencode-go --model
deepseek-v4-flash --effort high`. Precedence is CLI override > durable
session profile > config `[inference]` > built-in default. A resumed session
restores its profile and resolves credentials freshly from the environment or
local store.

Reasoning effort is a model capability, not a provider-wide constant. The
neutral set is `none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`, and each
model exposes only its documented subset: OpenAI models use the Responses
transport (required for tool calling with reasoning effort on GPT-5.4 and
later) with 1,050,000-token public API context windows and encrypted
reasoning captured from the stream for exactly-once stateless replay;
Anthropic models use
adaptive thinking with exact thinking/redacted-block replay; DeepSeek's
canonical API models are `deepseek-flash` and `deepseek-v4-pro` (Chat
Completions, `none`/`low`/`high`/`max`, required `reasoning_content` replay,
retired names accepted as aliases only); and OpenCode Go resolves transport,
reasoning efforts, and capabilities per model from its current documented list
(GPT/Grok/Muse Spark over Responses, MiniMax/Qwen over Messages, the rest over
Chat Completions). Unknown models stay conservative:
provider-default effort only, no replay assumption, no invented context
window or pricing. Advanced users can override context window, efforts,
default effort, replay policy, aliases, pricing, transport, and adaptive
thinking per model under `[providers.<id>.models.<id>]`.

`keyring:NAME` credentials parse for forward compatibility but are not
available in this build; `/setup` offers environment variables and the 0600
local secret store only.

Run one prompt without the TUI with `latch -p "Explain this repository"`.
Resume with `latch --resume`. One matching workspace session resumes directly;
several open an interactive newest-first picker with search, workspace/all
scope, metadata, and a lazily loaded transcript preview. Non-interactive use
never opens the picker and never guesses:

```sh
latch --resume --session 550e8400       # exact UUID or unique prefix
latch --resume --latest                 # deliberate newest workspace session
latch --resume --session <uuid> -p "continue and verify"
```

An ambiguous or missing prefix is an error. Selecting a session from another
workspace clearly changes Latch to that session's persisted workspace. Resume
restores the visible transcript, effective mode (override with `--mode work`),
task state, evidence, failure streaks, provider session UUID, continuity, and
change ownership without re-running historical tools. Session metadata lives
under `~/.latch/` by default, with the database at `latch.sqlite3` and large
output under `artifacts/`. Existing XDG installs (`~/.config/latch/config.toml`
and `~/.local/state/latch/`) continue to run in place until `latch migrate`
explicitly copies them. Migration keeps backups and records its source in
`~/.latch/migration.toml`. Removing the new config after migration starts fresh
setup; it does not reopen the old config.

Child agents are independent sessions and are omitted from the ordinary root
session picker. Resuming a root reconstructs its durable child graph; a child
whose turn was running when the process ended becomes `Interrupted` and can be
continued explicitly rather than silently rerunning work.

## Machine interface

`latch run`, `latch resume`, `latch sessions`, and `latch doctor` are the
non-interactive surface for scripts, CI, coding agents, and benchmark
harnesses. They never open the TUI, never wait for a human, and never mix
diagnostics into stdout.

```sh
latch run \
  --workspace ./fixture \
  --prompt "Fix the failing test" \
  --model deepseek-v4.1 \
  --output json
```

Prompt sources are exclusive: exactly one of `--prompt`, `--prompt-file`, or
`--stdin`.

```sh
cat task.txt | latch run --workspace ./fixture --stdin --output jsonl
```

`--workspace` resolves and canonicalizes the directory before session
selection, policy, prompt compilation, and tools, so callers never need to
`cd` first. `--output text` (the default) preserves the historical one-shot
behavior and streams assistant text to stdout. `--output json` prints exactly
one versioned object (`"schema_version": 3`) carrying the command, session id,
workspace, run status, resolved provider/model/effort, durable task state,
result text, scoped usage, context accounting, and event/graph summaries.
`--output jsonl` streams semantic records (`text_delta`, `tool_result`,
`durable_event`, `final`); every line is standalone JSON and an orderly
terminal state always ends with the `final` record. Logs and diagnostics
always go to stderr, so stdout stays parseable even with `RUST_LOG` set.

Two distinctions matter for benchmark consumers:

```text
run status != durable task completion
root usage != graph usage
```

`status` describes the invocation only: `status=completed` means it terminated
normally, not that the task is done. Use `task.completion` for Latch's durable
kernel state (`in_progress`, `implemented_not_verified`, `verified`, or
`blocked`), alongside `task.goal`, `task.evidence_count`, and a
`task.validation` passed/failed/pending summary.

Usage is scoped to the current CLI invocation. `usage.scope` is
`"invocation_graph"`; `usage.root` aggregates durable `ModelUsage` events the
root session emitted during this invocation, while `usage.graph` adds the same
for every durable child session of that root. Each session counts only events
emitted after a per-session durable sequence mark captured when the invocation
began, so a resumed root or child never re-reports an earlier run's tokens and a
child spawned mid-invocation counts from its own beginning. Provider categories
that were never reported stay `null` instead of being fabricated as zero; the
durable `model_usage` events remain the session's full lifetime record.
`events.scope` is always `"root"`: its range and count cover the root session's
stream only. `agent_graph` reports `sessions`, `child_sessions`, and total
graph `events` without exposing child transcripts.

Durable sessions can be inspected without the TUI or a TTY:

```sh
latch sessions list --workspace ./fixture --output json
latch sessions show 550e8400 --output json   # exact UUID or unique prefix
```

Exit codes are stable and deliberately small; the structured result remains
the authoritative detail:

| Code | Meaning |
| ---- | ------- |
| 0 | completed |
| 1 | runtime failure: storage, tool/extension setup, provider startup, or the run itself |
| 2 | CLI/configuration error: arguments, config, provider/model/profile, workspace, prompt source, session selector |
| 3 | permission unresolved: denied, never auto-approved |
| 4 | cancelled |

A `SIGINT` (Ctrl+C) or `SIGTERM` during a machine run cancels it and still
emits the final structured result with status `cancelled` (exit 4), so
Docker/CI stops and benchmark harnesses never lose the artifact.

Machine mode is fail-closed for permissions. The TUI remains the only
interactive approval surface; when an operation reaches an `Ask` that the
configured policy cannot resolve without a human, it is durably denied, the
final status is `permission_denied`, and the process exits 3. There is no
`--yes` or skip-permissions flag. Existing invocations keep working: `latch`
still launches the TUI, and `latch -p "..."` (also with `--resume`) now runs
through the same machine implementation, so profile precedence, session
construction, attachments, and policy are identical everywhere.

## Preflight doctor

`latch doctor` is a read-only preflight for the machine a run depends on. It
checks the mandatory Bubblewrap sandbox (by probing a minimal sandboxed
command), the `rg` search runtime, Git, the effective config path and state
directory, the resolved provider/model/effort, and that the provider credential
resolves. It never contacts the provider, never runs a task, and never prints
secret material: a credential is reported only by its symbolic reference
(`env:NAME`, `file:NAME`).

```sh
latch doctor
latch doctor --output json                                  # one versioned object for CI
latch doctor --provider opencode-go --model deepseek-v4-flash
```

Each check reports `ok`, `warn`, or `fail` with an actionable detail, and the
report names the workspace, config path, state directory, and resolved profile.
Exit codes are stable: `0` when every required check passes, `1` when a runtime
prerequisite is missing or unusable, and `2` for a CLI or configuration error
(bad workspace, unreadable config, unresolvable provider/profile, or a missing
credential).

## Terminal interface

When no usable provider is configured, the guided `/setup` flow opens on the
first run. Its active field shows the current value, accepts paste and ordinary
editing, uses Enter to confirm and Esc to go back, and masks secrets in both
the display and debug formatting.

The transcript is a semantic conversation rather than a kernel event log.
User-authored messages sit on a full-width neutral band with a `›` gutter,
while model output stays on the terminal background behind a quiet `•` bullet
and clean Markdown, so the two are distinguishable at a glance. Inspection
calls coalesce into an updating exploration cell; commands, edits, and
validation have dedicated lifecycle cells. Successful routine work stays
compact, while failures retain a bounded diagnostic:

```text
› Fix the failing tests without changing the public API.

• Explored
  └ Read Cargo.toml, src/lib.rs, tests/math.rs
    Search "average" in src/

• Edited src/lib.rs  +2 −2
    @@ -12,7 +12,7 @@
         let base = 10;
    -    base + a + b
    +    base + a - b
         base + a + b + offset

✓ Verified
  └ cargo test · 3 tests passed · 0.42s
```

Edit cells carry the real unified diff computed by the kernel from the actual
before/after bytes: additions and deletions keep their semantic green/red and
gain a restrained tinted background where the terminal supports it, unchanged
context stays neutral, and hunk/file metadata is dimmed rather than saturated.
Previews are compact by default and bounded with an explicit
`… N diff lines omitted · /diff for the full diff` note, and newly created,
deleted, repeated, multi-file, and Unicode content all render the same way. The
`+N −N` summary remains, but the diff itself is never reconstructed from those
counters.

The composer is the main control surface: a full-width neutral band (shared
with user messages and bottom action surfaces) with a `›` prompt gutter,
comfortable padding, a placeholder, mode/model/branch metadata, and subdued
keyboard hints. It grows with the prompt up to a fraction of the terminal
height and is backed by a real scrollable viewport, so a prompt pasted as
hundreds of lines can be inspected from anywhere before submission. When the
sidebar is hidden, the composer also carries a compact `≈tokens/window`
working-set summary. While a turn runs, a compact transient status row above
the composer reports the current activity from authoritative state
(`Exploring`, `Editing`, `Running tests`, `Validating`, `Waiting for
approval`, child-agent activity); completed work lands in the transcript
instead.

On an empty session a restrained welcome state keeps the composer as the
focus; it disappears once the conversation starts. Assistant responses render
headings, paragraphs, lists, fenced and inline code, bold/italic text, links,
URLs, and simple Markdown tables. ANSI and progress control sequences are
normalized. Ctrl+T or `/raw` toggles a copy-friendly detailed transcript;
`/diff` deliberately shows the complete bounded workspace diff (with artifact
spill for very large output). The palette is terminal-aware: it detects
truecolor/ANSI-256/ANSI-16 and light/dark via `COLORTERM`, `TERM`,
`COLORFGBG`, and the `LATCH_THEME`/`LATCH_COLOR` overrides, and drops
backgrounds entirely on ANSI-16 rather than guessing the palette.

## Observability sidebar

On wide terminals the transcript gets a right sidebar: session/model and turn
count, the latest durable user request, a token-native **working set** estimate
against the model context window (never "time until context death"), kernel
canonical task state and completion, separate run and validation status,
a compact **children** section for root-visible child sessions (durable
delegation and reports only, never a child transcript), provider-neutral usage
totals, and change ownership. It is responsive: ~32% on
very wide screens (clamped 28–44 columns), ~27% at 130–159, a compact sidebar
at 110–129, and hidden below 110 columns. `Ctrl+B` or `/sidebar` toggles it;
the transcript takes the full width when it is hidden. The sidebar is derived
only from durable kernel events, so live and resumed sessions show the same
state. Abnormal states (over-budget context, stalled progress, externally
modified owned files) are highlighted; healthy states stay quiet.

`/context` shows advanced accounting. The default sidebar omits cache
architecture, epoch, reserve, and headroom details. Advanced telemetry
distinguishes three quantities: estimated architecture
cacheability (shared prefix / request under Latch's own serialization and
estimator, not the provider's tokenizer), provider prefix utilization (cache
reads / shared prefix), and the measured provider cache hit rate (cache reads /
(reported hits + misses)). Provider-reported usage is authoritative, and
unknown categories stay unknown. Usage keeps cache hit, miss, and total input
distinct, so cost never double-charges cached tokens.

The provider-visible conversation is append-only within a durable cache epoch;
kernel state and recalled material are durable messages, so ordinary turns do
not rewrite the reusable prefix. When the working budget fills, one hysteretic
rotation keeps a large whole-unit working set and emits a fresh authoritative
snapshot instead of evicting a little every turn. Cache epochs are performance
boundaries only: raw events, canonical state, archival episodes, and recall are
independent of them, rotation never deletes memory, and `/compact` remains the
explicit reset. `/context` shows epoch generation, span, last rotation reason,
and retained tokens.

Context budgets and every user-visible context number are tokens, estimated
conservatively per provider/model; bytes remain only for internal file,
artifact, log, and I/O limits. The context window defaults to 256k tokens and
is overridable per model:

```toml
[models.deepseek-flash]
context_window_tokens = 262144
```

Usage is normalized per provider into input, output, and optional cache
read/write categories. A category the provider did not report shows `—`, never
a fabricated zero. Optional per-model pricing in `config.toml` produces a
clearly labeled **estimated cost**; Latch never fetches or invents prices:

```toml
[models.deepseek-flash.pricing]
input_per_million = 0.28
output_per_million = 0.42
cache_read_per_million = 0.028
currency = "USD"
```

## Validation is kernel-owned

In WORK, the model asks for validation by intent:

```json
{ "requirement": "existing unittest passes",
  "command": "python3 -B -m unittest test_calc -v" }
```

The kernel executes the command, records the result, links the evidence to real
provenance, and derives completion. The model never supplies or sees internal
identifiers and cannot self-certify a passing validation. A requirement that
failed and now passes supersedes the failure; historical attempts stay in the
raw event log. Completion states: `InProgress`, `ImplementedNotVerified`,
`Verified`, `Blocked` (a required validation could not run).

Passing evidence is tied to the workspace generation it checked. Guarded edits
and write-capable shell commands advance that generation, so an older pass
remains auditable but cannot make changed work `Verified`. Read-only inspection
preserves the pass; validation after a change restores it. Resume rebuilds the
same generation from durable events. A running write-capable managed process
also prevents a pass from certifying completion until it exits and validation
runs again. The generation covers all Latch sessions sharing the workspace,
including child agents. Validation runs serially with guarded edits and
synchronous shell commands; a pass overlapping another workspace writer stays
stale.

Completion state and loop termination are separate. `Verified` is the only
completion that ends the run on the strength of its own claim;
`ImplementedNotVerified` stays a real and honestly-labelled state, but it is
not verified success. When a run actually changed the workspace — judged from
durable change events, never from the model's own `touched_files` — and is
about to finish with an implementation claim and no passing validation, the
kernel spends one corrective turn through the same durable kernel-context
channel as progress re-ground: validate the change, or record why verification
is unavailable. One opportunity per run, never a loop. A run that mutated
nothing (read-only, explanatory, or documentation work) needs no validation and
finishes exactly as before, and no correction can ever promote an unverified
run to `Verified` — only kernel validation evidence does that.

## Durable child agents

The root model has seven stable control tools: `spawn_agent`,
`send_agent_message`, `continue_agent`, `wait_agents`, `list_agents`,
`interrupt_agent`, and `close_agent`. Spawning returns immediately and runs a
real child session asynchronously. Delegation is proportional: children are for
parallelizable, isolated, or independent workstreams, never simple, sequential,
or tightly coupled work. The child starts with a compact delegation
brief plus the workspace's normal repository instructions, not a copy of the
parent transcript. The first release permits depth 1 only; children see the
same fixed schema but the kernel rejects agent control from a child.

Each child owns its conversation, cache epoch, canonical task state, evidence,
failure/progress supervision, and lifecycle. Its compact `AgentReport` carries
semantic findings, touched files, child-only validation references, and open
questions. Reports enter the parent context only at a safe model boundary, and
child evidence never certifies root completion. Workspace mutation coordination
and the live parent capability ceiling are shared, while per-call grants and
managed processes remain isolated. `interrupt_agent` cancels only the current
turn and leaves the child reusable; `close_agent` shuts it down permanently.

## Agent groups

Agent groups add durable coordination on top of child agents without changing
how they run. `AgentGraph` stays topology, `AgentSupervisor` stays execution,
and one optional root-scoped `AgentGroup` adds a shared task DAG, atomic
claims, a durable peer mailbox, and replayable progress state. The group never
owns workers; ordinary single-agent and subagent use is unchanged, and group
state is created lazily on first use.

Three fixed tools expose coordination: `group_task` (create, list, claim,
start, complete, block, release, cancel), `group_message` (send, list), and
`group_status` (one compact snapshot). The root creates tasks and dependencies
as a validated DAG, then delegates ready work with
`spawn_agent { task_id }`, which joins the child and assigns the task
atomically. Exactly one agent can claim a task; only the assignee can start,
complete, block, or release it, and only the root can cancel or reassign.
Completing a task is coordination truth, never root evidence.

Messages are concise, durable, and information-only: they are delivered at the
recipient's next safe model boundary, exactly once across restarts, and never
wake an idle child or copy transcripts. Root terminal `complete` is refused
while required group tasks are still pending, claimed, in progress, or blocked;
optional and cancelled tasks never block. An interrupted child keeps its task
until an explicit release, reassignment, or completion, so crashes cannot
duplicate work. Workspace conflicts are surfaced as advisory warnings from
declared paths, never as hard blocks, and groups remain coordinated concurrency
— no autonomous scheduler and depth stays one. The TUI shows a compact `GROUP`
block and `/group` prints the full coordination view.

## Bounded reads, artifacts, and long processes

`read_file` and `read_artifact` read at most 64 KiB per call, decode at most
64 KiB of UTF-8, and persist at most 24 KiB and 8000 estimated tokens of
output. Their line windows support `offset`/`limit`/`tail`. A long line is
paged within the line; copy `offset`, `byte_offset`, and `cursor_line` from its
continuation marker. Large files report an unknown total line count. Files
that fit in one read keep their exact version hash and normal line ranges;
larger files omit a guarded-edit hash because hashing them would exceed the
read budget. Binary and invalid UTF-8 input is rejected.

`search` returns a bounded result page with a total count and offset
continuation. Output spilled by truncated shell, search, diff, or
validation results carries an artifact id that `read_artifact` can page through
by range. Long-running development commands use `exec_start`, `exec_poll`, and
`exec_terminate` instead of blocking shell calls; process lifecycle is durable,
and a resumed session reports honestly when a child did not survive restart.

## Image input and vision

When the selected model accepts image input, Latch can actually show it
pictures — both images the user attaches and screenshots the agent inspects
from the workspace:

- `/attach <path>` validates and ingests a PNG, JPEG, or WebP image (up to
  5 MiB and 8000 px per side) into the session's immutable artifact store and
  shows it as a pending attachment above the composer. `/attachments` lists
  pending images and `/detach <index|all>` removes them. The CLI accepts
  repeatable `--attach <path>` (alias `--image`) alongside `-p`, or to preload
  the first interactive prompt.
- The `read_image` tool inspects an image file in the workspace through the
  same ingestion path. `read_file` refuses binary images and points to
  `read_image` instead of dumping bytes; neither tool OCRs or approximates the
  image — the model sees the original pixels.
- The next model request carries the image: OpenAI Responses uses `input_image`
  content parts (user turns and `function_call_output`), Anthropic Messages
  uses base64 `image` blocks (user turns and inside `tool_result` content), and
  OpenAI-compatible chat completions uses the multimodal `image_url` content
  array with an adjacent observation turn for tool output.
- Images are durable. Events and messages store a compact `MediaRef` (content
  hash, MIME type, dimensions, artifact path), never base64 or raw bytes.
  Resume replays the exact original artifact even if the source file changed or
  was deleted.
- Image input is explicit model capability metadata (`input_modalities`),
  resolved with precedence user configuration > built-in catalog > conservative
  text-only fallback. If the selected model cannot accept images, Latch fails
  locally with a clear message instead of dropping them; `/model` marks
  image-capable models with `vision`. OpenCode Go publishes no per-model
  modalities, so every Go model is conservative text-only by default even
  though live probing found `kimi-k3` (chat completions) and `qwen3.8-max`
  (messages) do accept images; opt them in with
  `input_modalities = ["text", "image"]` in that provider's model table.
- Context accounting prices images as estimated visual tokens, never as base64
  text length; provider-reported usage stays authoritative. Historical images
  are resent inline on later requests in this version, so payload size grows
  with image-bearing history; provider-side Files API caching is a later
  optimization. Audio, video, image generation, and PDF understanding are not
  supported yet.

## Inspection loops are bounded

The kernel also supervises inspection. Reads, searches, git status/diff, and
conservative read-only shell observations are tracked by subject and result
digest per progress epoch; range arguments are part of the identity, so reading
a different window is new information. Mutations, external edits, validation
evidence, meaningful task-state changes, and new user turns advance the epoch,
so re-reading after real change is always allowed. Only consecutive turns that
repeat unchanged observations trigger a kernel-owned re-ground instruction that
lists what is already known; repeats after that are suppressed cleanly instead
of burning tool cycles. Long productive tasks are never killed by turn count:
the old 32-turn ceiling is gone, and `failure.max_model_turns` is an optional,
off-by-default circuit breaker (`failure.stagnation_budget`, default 2).

## Human approval for `Ask`

When policy asks for approval (for example an outside-workspace write with
`outside_workspace = "ask"`), the kernel emits a durable approval request and
pauses. The TUI shows a bottom action surface above the composer, leaving the
transcript visible: it names the tool, shows a readable command preview, the
reason, and the requested capability, with selectable Approve/Deny actions.
`y` approves, `n`/Esc denies, Enter confirms the highlighted action, and
Ctrl+O opens a scrollable full-request inspector instead of truncating what is
being approved; Ctrl+C cancels. Approval is single-use and keyed to a kernel
call id the model never sees, so the model cannot fabricate consent.
Non-interactive sessions record an explicit denial instead of hanging, and
resume marks requests that were pending at exit as expired. Dangerous shell
commands remain denied.

## Safety, permissions, and the sandbox

Mode, Safety, and Permissions are three orthogonal controls:

- **Mode** decides what kind of work is allowed: `ASK` and `PLAN` are read-only,
  `WORK` may mutate according to Safety.
- **Safety** (`/safety`: Strict, Standard, Autonomous) classifies each proposed
  capability as Allow, Ask, or Deny. Strict asks before workspace writes,
  Standard allows ordinary source edits, Autonomous also pre-grants network.
- **Permissions** (`/permissions`: Ask for approval, Approve for me, Auto
  approve) decides how an Ask is resolved; the selector marks the active mode
  and shows modes as unavailable with a reason during a live turn.

The decision flow for every operation is:

```text
proposed operation
  -> mode eligibility
  -> safety classification (capability classes)
  -> Allow / Ask / Deny
  -> permission resolver for Ask
  -> single-use scoped capability grant
  -> mandatory Bubblewrap sandbox
  -> execution
```

External filesystem writes, Git metadata mutation, network access, and remote
side effects always become a kernel `Ask` first — even in Autonomous mode with
All approved. Auto-approval records the normal `PermissionRequested` /
`PermissionResolved` provenance; it never bypasses classification and never
disables the sandbox. Hard-denied operations (privileged, system-destructive)
stay denied regardless of resolver.

Latch is Linux-first and **requires the system `bwrap` (bubblewrap) binary**.
Every shell, `exec_start`, validation, and inspection command runs inside the
sandbox; Latch refuses to execute commands unsandboxed rather than falling back.
The sandbox binds the host root read-only, gives the workspace an explicit
read-only or writable mount (`.git` stays read-only unless Git metadata mutation
was granted), provides private `/tmp` and scratch build output, masks `~/.ssh`,
GPG/cloud/registry credentials, the resolved Latch state directory and any
symlinked `secrets.toml` target, and `/run` sockets, and isolates user, PID,
IPC, UTS, and network namespaces. `Approve for me` reviews shell commands with a
separate stateless model call that returns strict JSON (`low` approves;
`medium`/`high`/`critical` reject with one actionable sentence); non-command
asks fall back to human approval.

Threat model: the sandbox strongly contains ordinary coding-agent mistakes,
prompt injection, unintended host filesystem access, unauthorized network
access, and process interference. It is not a defense against kernel exploits,
all resource-exhaustion attacks, damage inside explicitly granted writable
roots, or capabilities deliberately exposed to a profile.

`ASK` is read-only question answering, but inspection is not artificially
restricted: pipelines, `awk`, `jq`, Python analysis, `cargo metadata`, `cargo
tree`, `cargo check`, and `cargo test --no-run` are welcome inside the
read-only sandbox, with build output redirected to private scratch. `PLAN`
permits deep read-only exploration. `WORK` permits policy-approved edits and
developer commands. Kernel policy denies workspace mutation in ASK and PLAN
regardless of model instructions.

Slash commands, in discovery order: `/mode`, `/safety`, `/permissions`,
`/resume`, `/model`, `/setup`, `/context`,
`/diff`, `/sidebar`, `/checkpoint`, `/undo`, `/compact`, `/raw`, `/help`,
`/quit`, `/exit`.
Typing `/` in an empty composer opens a palette above it; Ctrl+P opens the same
palette for any single-line composer without typing a slash. Filtering is live
and fuzzy; Up/Down or Ctrl+P/Ctrl+N moves selection, Tab completes, Enter
dispatches, and Esc dismisses while keeping the typed text. `/resume` makes a clean application-level transition through
the same picker as `latch --resume`. `/quit` and `/exit` are aliases and never
become model input or durable user messages. `/compact` resets the active
working set while retaining durable history and canonical state. `/model`
walks provider → model → effort in the bottom selector and applies the chosen
profile live; Esc preserves the current profile. `/setup` is the guided
persistent configuration flow. Both are disabled during an active turn, and
the composer metadata always shows the active model and effort together.

## TUI controls

- **Composer:** Enter submits, Ctrl+J or Alt+Enter inserts a newline (Ctrl+J is
  a literal line feed and reaches every terminal; some, like Windows Terminal,
  reserve Alt+Enter for fullscreen), Left/Right move the cursor, Home/End jump
  within the current line, Ctrl+Home/End jump to the start/end of the whole
  prompt, Ctrl+A/E also move to line edges, Ctrl+W deletes a word, Ctrl+U/K
  delete to line start/end. Bracketed clipboard paste preserves multiline text,
  CRLF, Unicode, and blank lines without submitting; Enter remains the only
  submit action. The editor wraps Unicode correctly and never truncates the
  buffer.
- **Live steering:** while a task is running the composer stays active.
  Submitting text (Enter) queues it as a normal user turn and acknowledges it
  with a single `· steering queued` line; the one agent loop injects queued
  messages in order at the next safe model boundary — never inside an
  unresolved tool transaction — so the in-flight model request, tool, or
  managed process is never interrupted. Injected turns are durable user
  messages with normal provenance, override earlier plan decisions, and drive
  retrieval of older material into the volatile context tail. A steer that
  races the end of the run is either consumed by that run or handed back and
  sent as a new request; it is never left queued. Remaining not-yet-started
  side-effecting calls from the old plan are superseded with a terminal result
  so the model re-plans. Ctrl+C remains the only cancellation.
- **Images:** `/attach <path>` ingests a PNG/JPEG/WebP image as a pending
  attachment shown above the composer; `/attachments` lists pending images and
  `/detach <index|all>` removes them. Sending attaches them to that one user
  turn, and they can be attached to a steering message while a turn is running.
  A known text-only model refuses the send locally and keeps the attachment
  intact until a vision-capable model is selected with `/model`.
- **Composer scrolling:** when the prompt overflows the visible editor,
  PageUp/PageDown move through it, as does the mouse wheel over the composer.
  Somewhere-hidden content is marked with `↑`/`↓`/`↕`; the cursor stays visible
  as you type and navigation re-pins the viewport.
- **History:** Up/Down at the first/last composer line recall previous prompts;
  the draft returns past the newest entry; recalled entries are never mutated.
- **Transcript scrolling:** Shift+PageUp/PageDown, Shift+Home/End, and the mouse
  wheel outside the composer. Auto-follow resumes at the bottom; a subtle hint
  shows when newer content is below.
- **Cancel/quit:** Ctrl+C cancels a running turn, or quits when idle.
- **Permission:** when a tool needs approval, the bottom action surface above
  the composer shows the operation, the requested capability class, the reason,
  and a readable argument preview; Up/Down select, Enter confirms, `y` approves,
  `n`/Esc denies, Ctrl+O inspects the full request, and Ctrl+C cancels the turn.
  Approval is single-use and grants only the requested capability for that
  call.
- **Safety/permissions:** `/safety` and `/permissions` open restrained
  selectors above the composer; the effective short labels are shown in the
  composer metadata (for example `WORK · deepseek-v4-flash/low · main · std ·
  ask`) and both settings are restored exactly on resume.
- **Model/effort:** `/model` opens the live inference-profile selector and
  `/setup` the guided provider setup; both show only values the selected model
  supports and are unavailable during an active turn. `/setup` also supports
  adding, editing, and removing provider instances: removal asks for one
  confirmation, keeps stored credentials, and refuses to remove the only
  configured provider so `[inference]` never dangles.
- **Run metrics:** the sidebar shows a RUN block for the current/last user
  request next to cumulative SESSION totals. Reasoning-replay, tool-argument,
  and tool-result estimates are run-cumulative with the most recent request
  shown separately.
- **Child profiles:** a child is permanently associated with the inference
  profile it was spawned under; a root profile switch affects only future
  children, and a rebuilt or resumed child returns to its own pinned profile.
- **Sidebar:** Ctrl+B or `/sidebar` toggles the responsive state sidebar; when
  an agent group is active it shows a compact `GROUP` block with task counts
  and active owners.
- **Group:** `/group` prints the coordination view: task states, owners,
  dependencies, and dependency/conflict warnings.
- **Detail:** Ctrl+T or `/raw` toggles the detailed, copy-friendly transcript.
- **Diff:** `/diff` opens a full-width semantic diff inspector (red deletions,
  green additions, dim metadata). Scroll with Up/Down, PageUp/PageDown,
  Home/End; Ctrl+T switches between semantic and raw; Esc closes. The
  transcript keeps only a compact diff cell so large diffs never flood it.
- **Resume picker:** type to search, Up/Down and PageUp/PageDown navigate, Tab
  toggles current-workspace/all sessions, Enter resumes, Ctrl+F starts fresh,
  Ctrl+Q exits, and Esc cancels.
- `/help` prints the current control summary.

## Acceptance testing

The deterministic Linux release gate is one command:

```sh
bash scripts/release-gate.sh
```

It probes Bubblewrap and runs formatting, locked Clippy, kernel invariants,
the full workspace test suite, and a locked release build. CI runs this same
gate after installing Bubblewrap, ripgrep, and Python and enabling user
namespaces on its ephemeral Ubuntu runner. Sandbox coverage fails
clearly when Bubblewrap cannot create the required namespaces. Live-model,
paid-provider, benchmark, and long-session stress tests remain separate and
are not counted as passing deterministic CI.

A separate opt-in harness calls the configured provider for real and is ignored
by default:

```sh
LATCH_LIVE_TESTS=1 cargo test -p latch-kernel --test live_acceptance \
  -- --ignored --nocapture
# one scenario:
LATCH_LIVE_TESTS=1 LATCH_LIVE_SCENARIO=small_bug \
  cargo test -p latch-kernel --test live_acceptance live_small_bug_fix \
  -- --ignored --nocapture
```

Scenarios cover a small bug fix, a medium multi-file change, a >10-file
refactor, a large source file with a large validation log, validation
fail → debug → pass, interrupt + resume, and an explicit >100-model-turn
long-horizon run. Each writes `target/live-acceptance/<scenario>.json` with
turns, tool calls, input/output/cache tokens, pre-request estimated context,
provider-reported usage, validation outcomes, completion, and elapsed time.

## Docker dogfood harness

An optional, non-privileged Docker environment runs the machine CLI against a
disposable workspace, for manual dogfooding (including on a Raspberry Pi 5) and
deterministic, credential-free integration checks:

```sh
docker build --target runtime -t latch-dogfood:local -f docker/Dockerfile .
./scripts/dogfood-test.sh                 # deterministic, no network/keys/TTY
cp docker/config.dogfood.toml /tmp/latch-dogfood.toml   # configure a provider
OPENCODE_API_KEY=… ./scripts/dogfood.sh --config /tmp/latch-dogfood.toml \
  --provider-env OPENCODE_API_KEY "Fix the failing test and run it"
```

The harness only prepares the image, workspace, mounts, and explicitly
forwarded environment; it drives the same `latch run`/`resume`/`sessions`
machine interface. Credentials are never baked into the image and the
container runs non-root without `--privileged` or `--network host`. See
[docs/DOGFOOD_DOCKER.md](docs/DOGFOOD_DOCKER.md) for the trust boundary, mount
behavior, cleanup, ARM64 notes, and limitations.

## Extensions

Extensions are explicitly configured executables speaking JSON-RPC 2.0 over
LSP-style framed stdio. See [docs/PROTOCOL.md](docs/PROTOCOL.md), the minimal
TypeScript SDK under `sdk/typescript`, and `extensions/example-ts`. The runtime
platform model that governs extensions, replaceable backends, and future
clients is [docs/RUNTIME_CAPABILITY_MODEL.md](docs/RUNTIME_CAPABILITY_MODEL.md).
V0.2.2 runs
the extension host inside the mandatory sandbox (read-only workspace, masked
home and resolved Latch state directory, network for protocol work); extension
tool arguments remain a cooperative
audit contract, and no syscall isolation is claimed inside the host. Extension
lifecycle stages are bounded end to end by the central `[extension_lifecycle]`
policy: spawn, initialize, registration/ready, ordinary RPCs, shutdown, and
graceful exit each have a configurable deadline (defaults are documented in
`config.example.toml`). A missed deadline kills and reaps the child, and
extension startup observes the run's cancellation token, so a silent or broken
extension can never block Latch startup, RPC handling, cancellation, or
shutdown.

Build and probe the reference extension with:

```sh
(cd sdk/typescript && npm ci && npm run build)
(cd extensions/example-ts && npm ci && npm run build)
cargo run -p latch-kernel --example extension_probe -- \
  extensions/example-ts/dist/index.js
```

Latch v0.2.2 supports Linux terminals only and requires the system `bwrap` binary. It has no daemon, browser automation,
remote execution, MCP, IDE integration, automatic commits, or automatic pushes.
