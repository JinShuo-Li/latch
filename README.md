# Latch

Latch is a quiet, programmable terminal coding agent built around explicit state,
evidence, controlled execution, and continuous long-session memory. V0.1 is a
Linux-first Rust implementation with a native streamed tool loop, durable SQLite
sessions, OpenAI-compatible and Anthropic providers, guarded coding tools, a
minimal Ratatui interface, and language-independent process extensions.

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
`latch/0.1`; OpenCode Go endpoints (`base_url` under `https://opencode.ai/zen/go`)
additionally receive a stable `x-opencode-session` header carrying the durable
session id, so `--resume` keeps the same value.

Run one prompt without the TUI with `latch -p "Explain this repository"`.
Continue the latest session for the current workspace with `latch --resume`.
Session metadata lives under `~/.local/state/latch/` by default; large output is
stored in its `artifacts/` tree.

## Modes and commands

`ASK` is read-only question answering. `PLAN` permits deep read-only exploration.
`WORK` permits policy-approved edits and developer commands. Kernel policy denies
workspace mutation in ASK and PLAN regardless of model instructions.

The TUI supports `/mode`, `/model`, `/context`, `/diff`, `/checkpoint`, `/undo`,
`/compact`, `/help`, and `/quit`. `/compact` resets the active working set while
retaining raw history, constraints, decisions, evidence, and task state. Normal
operation never performs traditional automatic compaction.

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
