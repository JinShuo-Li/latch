# AGENTS.md — Latch

Latch is a Linux-first Rust terminal coding agent (v0.2.0). Users drive it through
the `latch` TUI (`cargo install --path crates/latch-cli` installs it).

## Hard constraints

- `.references/` holds ignored, read-only upstream research checkouts (Pi, Codex).
  Never edit, commit, vendor, import, link against, or add a dependency on them.
  Latch is an independent implementation; revisions are pinned in `references/*.rev`
  and materialized by `scripts/fetch-references.sh`.
- First-party crates forbid unsafe code (`[workspace.lints.rust] unsafe_code = "forbid"`).
- Preserve unrelated and pre-existing workspace changes. Ground edits against
  observed file content/hashes. Never use destructive Git recovery (`checkout --`,
  `reset --hard`, `clean -f`) to undo.
- Do not add product features or change user-visible behavior unless asked; fix
  concrete bugs. Preserve resume/safety/provider semantics by default.
- Keep the OpenCode Go integration intact: endpoint behavior, the stable
  `x-opencode-session` header, and model selection are not to be changed.
- Keep provider-specific serialization in `crates/latch-kernel/src/provider.rs`:
  OpenAI-compatible reasoning replay (DeepSeek tool-call turns) and Anthropic
  role merging plus its system cache breakpoint must stay valid. Internal memory
  and event types remain provider-neutral.

## Commands

Exact gates CI runs; run all three before committing:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

- Single test: `cargo test -p latch-kernel --lib -- <name>`
- One integration file: `cargo test -p latch-kernel --test dogfood`
- `cargo test --workspace` is the slow one (~30s): sandbox/command tests spawn
  `bwrap`, and `search` tests need `rg`.
- Live-model acceptance is opt-in and ignored:
  `LATCH_LIVE_TESTS=1 cargo test -p latch-kernel --test live_acceptance -- --ignored --nocapture`
  (`LATCH_LIVE_SCENARIO=<name>` filters; reports land in `target/live-acceptance/`).
- Snapshot updates: `LATCH_UPDATE_SNAPSHOTS=1 cargo test -p latch-tui --lib`.
  Exception: `crates/latch-tui/tests/snapshots/v31_sidebar.txt` is `include_str!`,
  so edit it by hand and keep `cargo test -p latch-tui --lib` green.
- Extension fixture tests need `python3`; the reference TS extension needs
  `node` + `npm ci && npm run build` in `sdk/typescript` and `extensions/example-ts`.

Runtime prerequisites are mandatory, not optional: system `bwrap` (all command
execution is sandboxed; there is no unsandboxed fallback) and `rg` (the `search`
tool; it must fail with an actionable message, never bare ENOENT). State lives in
`~/.local/state/latch/`; config example is `config.example.toml`.

## Architecture map

Four crates: `latch-protocol` (durable event/model schema shared by all),
`latch-kernel` (agent loop, providers, tools, sandbox, continuity, SQLite store),
`latch-tui` (Ratatui app), `latch-cli` (wiring and `latch` binary).

Kernel ownership boundaries — put changes in the right child module:
`agent.rs` keeps the run loop and public facade, with `agent/{steering,request,
permissions,dispatch,kernel_tools,validation,supervision}.rs`; `tools.rs` keeps
`ToolExecutor` + dispatch, with `tools/{policy,ownership,process,files,write,git}.rs`.
`continuity.rs` is intentionally one module (rollover, episodes, recall share one
invariant). TUI: `lib.rs` is app state/reducer plus `{transcript,markdown,chrome,
runtime}.rs` and existing siblings.

Memory/cache invariants (do not violate):
- The raw event log is the source of truth and is never deleted or lossily
  summarized; canonical task state is authoritative; archival episodes and FTS
  recall stay independently available.
- Cache epochs are performance boundaries, not memory boundaries. Provider-visible
  history is append-only within an epoch; kernel context is durable `KernelContext`
  snapshot/delta events, and rotation is hysteretic, whole-unit, and replayable.
- Transitions that must survive resume fail closed: a live state change must not
  be reported successful before its durable event commits.

## Working preferences (maintainer)

- Implement the change and validate it; do not stop at a report or plan.
- Small, atomic Conventional Commits grouped by concern; stage only intended files.
- Push to `main` and confirm GitHub Actions is green (`gh run watch <run-id>`).
- "Build + release" means a local release: `cargo build --release --locked`,
  package the stripped binary as `target/dist/latch-v<version>-<host>.tar.gz`
  with `SHA256SUMS` and a `BUILD_INFO`, and `cargo install --path crates/latch-cli
  --locked --force`. Do not create Git tags or GitHub releases unless asked.
- Long-horizon task quality outranks cache locality; prefer explicit, deterministic
  behavior and durable provenance over hidden heuristics.
- When behavior, persistence semantics, or module ownership change, update
  `docs/ARCHITECTURE.md` / `docs/CONTINUITY.md` and the affected `README.md`
  section in the same change.
- After completing any work, update this `AGENTS.md` if the change altered
  commands, prerequisites, module ownership, constraints, or working
  preferences. Keep it compact and verified; it is guidance, not a changelog.
