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

The V3 transcript is a semantic conversation rather than a kernel event log.
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

Assistant responses render headings, paragraphs, lists, fenced and inline code,
bold/italic text, links, URLs, and simple Markdown tables. ANSI and progress
control sequences are normalized. Ctrl+T or `/raw` toggles a copy-friendly
detailed transcript; `/diff` deliberately shows the complete bounded workspace
diff (with artifact spill for very large output).

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

## Inspection loops are bounded

The kernel also supervises inspection. Reads, searches, git status/diff, and
conservative read-only shell observations are tracked by subject and result
digest per progress epoch. Mutations, external edits, validation evidence,
meaningful task-state changes, and new user turns advance the epoch, so
re-reading after real change is always allowed. Only consecutive turns that
repeat unchanged observations trigger a kernel-owned re-ground instruction that
lists what is already known; repeats after that are suppressed cleanly instead
of burning tool cycles, and the 32-turn limit remains a last-resort breaker
(`failure.stagnation_budget`, default 2).

## Modes and commands

`ASK` is read-only question answering. `PLAN` permits deep read-only exploration.
`WORK` permits policy-approved edits and developer commands. Kernel policy denies
workspace mutation in ASK and PLAN regardless of model instructions; validation
commands follow the same policy.

Slash commands, in discovery order: `/mode`, `/resume`, `/model`, `/context`,
`/diff`, `/checkpoint`, `/undo`, `/compact`, `/raw`, `/help`, `/quit`, `/exit`.
Typing `/` in an empty composer opens a palette above it. Filtering is live and
fuzzy; Up/Down or Ctrl+P/Ctrl+N moves selection, Tab completes, Enter dispatches,
and Esc dismisses. `/resume` makes a clean application-level transition through
the same picker as `latch --resume`. `/quit` and `/exit` are aliases and never
become model input or durable user messages. `/compact` resets the active
working set while retaining durable history and canonical state.

## TUI controls

- **Input:** Enter submits, Alt+Enter inserts a newline, Left/Right move the
  cursor, Ctrl+A/E jump to line start/end, Ctrl+W deletes a word, Ctrl+U/K
  clear to line start/end. The input area expands (bounded) with visible
  wrapping. Bracketed clipboard paste preserves multiline text, CRLF, Unicode,
  and blank lines without submitting; Enter remains the only submit action.
- **History:** Up/Down recall previous prompts; the draft returns past the
  newest entry; recalled entries are never mutated.
- **Scrollback:** PageUp/PageDown, Home/End, mouse wheel. Auto-follow resumes at
  the bottom; a subtle indicator shows newer content while scrolled up.
- **Cancel/quit:** Ctrl+C cancels a running turn, or quits when idle.
- **Detail:** Ctrl+T or `/raw` toggles the detailed, copy-friendly transcript.
- **Resume picker:** type to search, Up/Down and PageUp/PageDown navigate, Tab
  toggles current-workspace/all sessions, Enter resumes, Ctrl+F starts fresh,
  Ctrl+Q exits, and Esc cancels.
- `/help` prints the current control summary.

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
