# AGENTS.md — Latch

Latch is a Linux-first Rust terminal coding agent (v0.2.2). Users drive it through
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

CI is a small, stable architectural philosophy gate, not the full validation
suite. CI runs exactly:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p latch-kernel --test invariants
```

`crates/latch-kernel/tests/invariants.rs` is the deliberately selected fast,
deterministic invariant tier (no `bwrap`, `rg`, `python3`, network, timing, or
large histories). Keep it small and trustworthy; do not grow CI into a coverage
contest.

For significant changes, run the full local validation plus relevant
cache/long-session tests before committing; passing CI alone is not sufficient:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

- Single test: `cargo test -p latch-kernel --lib -- <name>`
- One integration file: `cargo test -p latch-kernel --test dogfood`
- CLI machine interface: `cargo test -p latch-cli` spawns the real binary
  against a loopback SSE mock provider with an isolated state dir; it needs no
  network, credentials, or TTY. It runs locally, not in CI.
- Preflight: `latch doctor` is a read-only check of `bwrap`, `rg`, Git, config,
  state dir, profile, and credential. It never contacts the provider; exit 0
  (all pass), 1 (runtime prerequisite), 2 (config/CLI). Keep it in sync when
  adding a prerequisite.
- `cargo test --workspace` takes ~12s: sandbox/command tests spawn `bwrap`,
  `search` tests need `rg`, and the multi-agent dogfood suites dominate the
  remainder. Keep process fixtures short-lived; a long natural lifetime makes
  teardown races expensive under a parallel suite.
- Cache/long-session stress (run locally, normally not in CI):
  `cargo test -p latch-kernel --lib continuity -- --nocapture`.
- Live-model acceptance is opt-in and ignored:
  `LATCH_LIVE_TESTS=1 cargo test -p latch-kernel --test live_acceptance -- --ignored --nocapture`
  (`LATCH_LIVE_SCENARIO=<name>` filters; reports land in `target/live-acceptance/`).
- Live effort benchmark is opt-in and ignored:
  `LATCH_LIVE_TESTS=1 cargo test -p latch-kernel --test live_benchmark -- --ignored --nocapture`
  (`LATCH_BENCH_PROVIDER` / `LATCH_BENCH_MODEL` / `LATCH_BENCH_EFFORTS` filter;
  reports land in `target/live-benchmark/`).
- Snapshot updates: `LATCH_UPDATE_SNAPSHOTS=1 cargo test -p latch-tui --lib`.
  Exception: `crates/latch-tui/tests/snapshots/v31_sidebar.txt` is `include_str!`,
  so edit it by hand and keep `cargo test -p latch-tui --lib` green.
- Extension fixture tests need `python3`; the reference TS extension needs
  `node` + `npm ci && npm run build` in `sdk/typescript` and `extensions/example-ts`.
- Docker dogfood harness (local, not in CI): `./scripts/dogfood.sh "<task>"`
  runs the machine CLI in an isolated, non-root container against a disposable
  workspace; `./scripts/dogfood-test.sh` builds the `test` image stage and runs
  the deterministic, credential-free integration checks. See
  `docs/DOGFOOD_DOCKER.md`.

Runtime prerequisites are mandatory, not optional: system `bwrap` (all command
execution is sandboxed; there is no unsandboxed fallback) and `rg` (the `search`
tool; it must fail with an actionable message, never bare ENOENT). State lives in
`~/.local/state/latch/`; config example is `config.example.toml`. Docker (or a
compatible CLI via `DOCKER=…`) is required only for the optional dogfood harness,
never by Latch itself.

## Testing philosophy

> Memory decides what the model needs to know. Cache decides how cheaply we can
> send it.

> CI protects what Latch must never stop being. Local tests verify that the
> current implementation actually works.

Three tiers:

- **CI (architectural invariants):** durable history is the source of truth,
  cache epochs are not memory boundaries, canonical state stays authoritative,
  no hidden destructive compaction, resume equivalence, kernel-owned
  validation/evidence, safety/sandbox rules, steering/tool protocol
  correctness, append-only cache-epoch behavior, deterministic serialization.
- **Local (`cargo test --workspace`):** detailed correctness, providers,
  sandbox/command execution, snapshots, extensions. Expected before significant
  commits.
- **Stress (local, ignored/opt-in by convention):** long-session, large-history,
  scaling, and extreme behavior. Normally stays out of CI.

Do not move tests between tiers merely to make CI green. Keep expensive, flaky,
network-dependent, timing-sensitive, large-history, provider, benchmark, and
stress tests out of CI. Passing CI alone is insufficient for substantial
changes. Cache locality must never override long-horizon correctness.

## Architecture map

Four crates: `latch-protocol` (durable event/model schema shared by all),
`latch-kernel` (agent loop, providers, tools, sandbox, continuity, SQLite store),
`latch-tui` (Ratatui app), `latch-cli` (wiring and `latch` binary).

Kernel ownership boundaries — put changes in the right child module:
`agent.rs` keeps the run loop and public facade, with `agent/{steering,request,
permissions,dispatch,agent_controls,kernel_tools,group_tools,validation,
supervision}.rs`;
root-scoped child ownership is in
`agents/{supervisor,worker,graph,mailbox,profile,group}.rs`. `tools.rs` keeps
`ToolExecutor` + dispatch, with `tools/{policy,ownership,process,files,write,git}.rs`.
`context.rs` owns the replaceable `ContextEngine` port (request/budget/view
objects and trait); `capability.rs` owns the kernel-declared runtime capability
vocabulary (kind/owner/lifetime/scope/permissions); `continuity.rs` is the
default engine implementation and intentionally one module (rollover, episodes,
recall share one invariant). `media.rs` owns image validation/ingestion and the
artifact media resolver; provider adapters serialize durable `MediaRef`s to
wire images.
TUI: `lib.rs` is app state/reducer plus `{transcript,markdown,chrome,
theme,runtime,agents,group}.rs` and existing siblings.

Runtime platform rules: kernel invariants, port rules, capability semantics,
transport independence, and the implemented-vs-deferred port map are normative
in `docs/RUNTIME_CAPABILITY_MODEL.md`. Keep the `ContextEngine` port narrow
(request/result objects, no `EventStore`/SQLite in the contract) and keep
provider-specific serialization in `provider.rs`.

Memory/cache invariants (do not violate):
- Memory decides what the model needs to know. Cache decides how cheaply we can
  send it. Cache epochs are performance boundaries, not memory boundaries, and
  cache locality must never override long-horizon correctness.
- The raw event log is the source of truth and is never deleted or lossily
  summarized; canonical task state is authoritative; archival episodes and FTS
  recall stay independently available.
- Provider-visible history is append-only within an epoch; kernel context is
  durable `KernelContext` snapshot/delta events, and rotation is hysteretic,
  whole-unit, and replayable.
- The provider `system` field is session-independent (behavioral core + kernel
  semantics only). Per-session context (workspace, repository instructions,
  mode) is the first provider-visible message, so `system` + tools stays
  cacheable across sessions and mode switches; keep `PromptCompiler`'s
  `stable`/`session` split and `ContextEngine`'s `session_context` intact.
- Transitions that must survive resume fail closed: a live state change must not
  be reported successful before its durable event commits.
- A Passed validation certifies only its durable workspace generation. Edits
  and write-capable commands from any session sharing the workspace make older
  passes stale; replay must derive the
  same completion state without deleting historical evidence. Active managed
  processes block certification until exit and revalidation.
- Child agents are independent durable sessions. Their task/evidence/continuity
  never becomes root truth; only semantic reports cross the boundary. Agent
  notifications are appended to root history only at safe model boundaries.
- Agent groups are an optional, root-scoped coordination overlay (shared task
  DAG, atomic claims, durable peer mailbox); they never own workers or replace
  the supervisor. Group SQLite projections must stay rebuildable from durable
  group events, claims stay transactional compare-and-set, and group completion
  is coordination truth, never root evidence.
- The initial agent architecture has maximum depth 1. All sessions retain one
  stable generic agent-control schema; child control attempts are denied.

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
- When behavior, persistence semantics, module ownership, or runtime
  port/capability semantics change, update `docs/ARCHITECTURE.md` /
  `docs/CONTINUITY.md` / `docs/RUNTIME_CAPABILITY_MODEL.md` and the affected
  `README.md` section in the same change.
- After completing any work, update this `AGENTS.md` if the change altered
  commands, prerequisites, module ownership, constraints, or working
  preferences. Keep it compact and verified; it is guidance, not a changelog.
