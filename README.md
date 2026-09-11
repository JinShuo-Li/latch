# Latch

Latch is a quiet, programmable terminal coding agent built around explicit state,
evidence, controlled execution, and continuous long-session memory. V0.1.1 is a
Linux-first Rust implementation with a native streamed tool loop, durable SQLite
sessions, OpenAI-compatible and Anthropic providers, kernel-owned validation
evidence, guarded coding tools, a modern Ratatui interface with a slash command
palette and real input editing, and language-independent process extensions.

Latch is independent software. Pi was studied as a public reference for agent
and terminal interaction behavior; Latch is not a fork and has no Pi dependency.

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
Continue the latest session for the current workspace with `latch --resume`:
the visible transcript replays, the effective mode restores (override with
`latch --resume --mode work`), and task state, evidence, failure streaks, and
change ownership survive. Session metadata lives under `~/.local/state/latch/`
by default; large output is stored in its `artifacts/` tree.

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

## Modes and commands

`ASK` is read-only question answering. `PLAN` permits deep read-only exploration.
`WORK` permits policy-approved edits and developer commands. Kernel policy denies
workspace mutation in ASK and PLAN regardless of model instructions; validation
commands follow the same policy.

Slash commands: `/mode`, `/model`, `/context`, `/diff`, `/checkpoint`, `/undo`,
`/compact`, `/help`, `/quit`. Typing `/` opens a live-filtered palette
(Tab/Enter complete, Esc closes). `/compact` resets the active working set while
retaining raw history, constraints, decisions, evidence, and task state. Normal
operation never performs traditional automatic compaction.

## TUI controls

- **Input:** Enter submits, Alt+Enter inserts a newline, Left/Right move the
  cursor, Ctrl+A/E jump to line start/end, Ctrl+W deletes a word, Ctrl+U/K
  clear to line start/end. The input area expands (bounded) with visible
  wrapping.
- **History:** Up/Down recall previous prompts; the draft returns past the
  newest entry; recalled entries are never mutated.
- **Scrollback:** PageUp/PageDown, Home/End, mouse wheel. Auto-follow resumes at
  the bottom; a subtle indicator shows newer content while scrolled up.
- **Cancel/quit:** Ctrl+C cancels a running turn, or quits when idle.
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
