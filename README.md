# Latch

Latch is a quiet, programmable terminal coding agent built around explicit state,
evidence, controlled execution, and continuous long-session memory. V0.1.1 is a
Linux-first Rust implementation with a native streamed tool loop, durable SQLite
sessions, OpenAI-compatible and Anthropic providers, kernel-owned validation
evidence, guarded coding tools, a modern Ratatui interface with a slash command
palette and real input editing, and language-independent process extensions.

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

For Anthropic, copy `config.example.toml` to
`~/.config/latch/config.toml`, set `provider.kind = "anthropic"`, choose a model,
and export `ANTHROPIC_API_KEY`. OpenAI-compatible servers can set `base_url` and
the environment variable named by `api_key_env`. Credentials are read from the
environment, never stored in a session or logged. Provider requests identify as
`latch/0.1.1`; OpenCode Go endpoints (`base_url` under `https://opencode.ai/zen/go`)
additionally receive a stable `x-opencode-session` header carrying the durable
session id, so `--resume` keeps the same value.

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
under `~/.local/state/latch/` by default; large output is stored in its
`artifacts/` tree.

## Terminal interface

The transcript is a semantic conversation rather than a kernel event log.
Inspection calls coalesce into an updating exploration cell; commands, edits,
and validation have dedicated lifecycle cells. Successful routine work stays
compact, while failures retain a bounded diagnostic:

```text
› Fix the failing tests without changing the public API.

• Explored
  └ Read Cargo.toml, src/lib.rs, tests/math.rs
    Search "average" in src/

• Edited src/lib.rs  +2 −2

✓ Verified
  └ cargo test · 3 tests passed · 0.42s
```

The composer is the main control surface: a closed rounded frame (cyan while it
owns input, quiet gray during overlays or approvals) with comfortable padding, a
placeholder, mode/model/branch metadata, live status (`ready`, `● working`,
`interrupted`, `approval needed`), and subdued keyboard hints. It grows with the
prompt up to a fraction of the terminal height and is backed by a real
scrollable viewport, so a prompt pasted as hundreds of lines can be inspected
from anywhere before submission. When the sidebar is hidden, the composer also
carries a compact `≈tokens/window` working-set summary.

On an empty session a restrained welcome state keeps the composer as the
focus; it disappears once the conversation starts. Assistant responses render
headings, paragraphs, lists, fenced and inline code, bold/italic text, links,
URLs, and simple Markdown tables. ANSI and progress control sequences are
normalized. Ctrl+T or `/raw` toggles a copy-friendly detailed transcript;
`/diff` deliberately shows the complete bounded workspace diff (with artifact
spill for very large output).

## Observability sidebar

On wide terminals the transcript gets a right sidebar: session/model and turn
count, a token-native **working set** estimate against the model context window
(never "time until context death"), kernel canonical task state and completion,
provider-neutral usage totals, and change ownership. It is responsive: ~32% on
very wide screens (clamped 28–44 columns), ~27% at 130–159, a compact sidebar
at 110–129, and hidden below 110 columns. `Ctrl+B` or `/sidebar` toggles it;
the transcript takes the full width when it is hidden. The sidebar is derived
only from durable kernel events, so live and resumed sessions show the same
state. Abnormal states (over-budget context, stalled progress, externally
modified owned files) are highlighted; healthy states stay quiet.

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

## Bounded reads, artifacts, and long processes

`read_file` returns a bounded line window (default 2000 lines) with
`offset`/`limit`/`tail` and an explicit continuation offset; large files are
never injected whole. `search` returns a bounded result page with a total count
and offset continuation. Output spilled by truncated shell, search, diff, or
validation results carries an artifact id that `read_artifact` can page through
by range. Long-running development commands use `exec_start`, `exec_poll`, and
`exec_terminate` instead of blocking shell calls; process lifecycle is durable,
and a resumed session reports honestly when a child did not survive restart.

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
pauses. The TUI shows a centered prompt: `y`/Enter approves, `n`/Esc denies,
Ctrl+C cancels. Approval is single-use and keyed to a kernel call id the model
never sees, so the model cannot fabricate consent. Non-interactive sessions
record an explicit denial instead of hanging, and resume marks requests that
were pending at exit as expired. Dangerous shell commands remain denied.

## Modes and commands

`ASK` is read-only question answering. `PLAN` permits deep read-only exploration.
`WORK` permits policy-approved edits and developer commands. Kernel policy denies
workspace mutation in ASK and PLAN regardless of model instructions; validation
commands follow the same policy.

Slash commands, in discovery order: `/mode`, `/resume`, `/model`, `/context`,
`/diff`, `/sidebar`, `/checkpoint`, `/undo`, `/compact`, `/raw`, `/help`,
`/quit`, `/exit`.
Typing `/` in an empty composer opens a palette above it; Ctrl+P opens the same
palette for any single-line composer without typing a slash. Filtering is live
and fuzzy; Up/Down or Ctrl+P/Ctrl+N moves selection, Tab completes, Enter
dispatches, and Esc dismisses while keeping the typed text. `/resume` makes a clean application-level transition through
the same picker as `latch --resume`. `/quit` and `/exit` are aliases and never
become model input or durable user messages. `/compact` resets the active
working set while retaining durable history and canonical state.

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
- **Permission:** when a tool needs approval, `y` approves and `n`/Esc denies;
  Ctrl+C cancels the turn.
- **Sidebar:** Ctrl+B or `/sidebar` toggles the responsive state sidebar.
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

Kernel invariants run offline under `cargo test` with the scripted provider. A
separate opt-in harness calls the configured provider for real and is ignored
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

## Extensions

Extensions are explicitly configured executables speaking JSON-RPC 2.0 over
LSP-style framed stdio. See [docs/PROTOCOL.md](docs/PROTOCOL.md), the minimal
TypeScript SDK under `sdk/typescript`, and `extensions/example-ts`. V0.1 treats
extension permission declarations as a cooperative audit contract; it does not
provide syscall isolation.

Build and probe the reference extension with:

```sh
(cd sdk/typescript && npm ci && npm run build)
(cd extensions/example-ts && npm ci && npm run build)
cargo run -p latch-kernel --example extension_probe -- \
  extensions/example-ts/dist/index.js
```

Latch V0.1 supports Linux terminals only. It has no daemon, browser automation,
remote execution, MCP, IDE integration, automatic commits, or automatic pushes.
