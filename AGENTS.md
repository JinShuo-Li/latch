# AGENTS.md — Latch

Latch is a Linux-first Rust terminal coding agent (v0.2.3). Users drive it through
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

Default CI (`.github/workflows/ci.yml`) is a fast Linux and Windows smoke gate:
formatting, kernel architectural invariants, a real Bubblewrap probe, an
embedded Windows boundary command, and a CLI build/start on both hosts. The
manual `Full validation` workflow runs the complete Linux release gate and
Windows native security and Rust runtime suites.

The deterministic Linux release gate remains `bash scripts/release-gate.sh`.
The manual CI workflow installs Bubblewrap, ripgrep, and Python and enables
user namespaces. The gate probes the required sandbox namespaces and mounts,
then runs:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test -p latch-kernel --test invariants --locked
cargo test --workspace --locked
cargo build --release --locked
```

`crates/latch-kernel/tests/invariants.rs` remains the fast architectural tier.
The full workspace suite covers deterministic F1–F5 regressions and sandbox
behavior; a failed Bubblewrap probe fails either gate rather than skipping
security coverage. Live/paid/network-provider tests remain opt-in and ignored.

For significant changes, run the release gate plus relevant cache/long-session
tests before committing:

```sh
cargo fmt --all -- --check
bash scripts/release-gate.sh
```

- Single test: `cargo test -p latch-kernel --lib -- <name>`
- One integration file: `cargo test -p latch-kernel --test dogfood`
- CLI machine interface: `cargo test -p latch-cli` spawns the real binary
  against a loopback SSE mock provider with an isolated state dir; it needs no
  network, credentials, or TTY. It is included in the workspace CI suite.
- Preflight: `latch doctor` is a read-only check of the execution backend, `rg`, Git, config,
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

Runtime prerequisites are mandatory, not optional: system `bwrap` on Linux or
the embedded native AppContainer runner on Windows (all command execution is
sandboxed; there is no unsandboxed fallback), and `rg` (the `search`
tool; it must fail with an actionable message, never bare ENOENT). State lives in
`~/.latch/` by default; legacy XDG installs stay in place until `latch migrate`
copies them. Config example is `config.example.toml`. Docker (or a
compatible CLI via `DOCKER=…`) is required only for the optional dogfood harness,
never by Latch itself.

## Testing philosophy

> Memory decides what the model needs to know. Cache decides how cheaply we can
> send it.

> Default CI protects architecture and basic runtime execution. Manual full
> validation protects deterministic release reliability. Local stress and live
> tests verify scale and real-provider behavior separately.

Three tiers:

- **Default CI (architectural invariants plus runtime smoke):** durable history is the source of truth,
  cache epochs are not memory boundaries, canonical state stays authoritative,
  no hidden destructive compaction, resume equivalence, kernel-owned
  validation/evidence, safety/sandbox rules, steering/tool protocol
  correctness, append-only cache-epoch behavior, deterministic serialization.
- **Deterministic workspace suite (manual CI and local):** detailed correctness,
  providers, sandbox/command execution, snapshots, extensions, and release build.
- **Stress (local, ignored/opt-in by convention):** long-session, large-history,
  scaling, and extreme behavior. Normally stays out of CI.

Keep the full tests and security assertions in the manual gate. Keep paid-provider,
network-dependent, flaky, benchmark, and stress tests out of CI. Cache locality
must never override long-horizon correctness.

## Architecture map

Four crates: `latch-protocol` (durable event/model schema shared by all),
`latch-kernel` (agent loop, providers, tools, sandbox, continuity, SQLite store),
`latch-tui` (Ratatui app), `latch-cli` (wiring and `latch` binary).
The static GitHub Pages site lives in `site/` and is published by
`.github/workflows/pages.yml`; its terminal captures come from TUI snapshots.
Keep its version and install commands aligned with the README.

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
`execution/` owns the platform backend boundary used by command-starting tools
and extension hosts. Linux delegates to the mandatory Bubblewrap runner in
`sandbox.rs`; Windows uses the embedded `native/windows/boundary/` runner,
`cmd.exe`, a per-call AppContainer, a write-restricted token, durable ACL
recovery, and a Job Object. It has no unsandboxed or local-account fallback.
The native runner is built by
`crates/latch-kernel/build.rs` on Windows (x64 MSVC C++ Build Tools + Windows
SDK required). Its Rust adapter is `execution/windows_runtime.rs` and its
read-only Python/Node staging is `execution/windows_runtime_tools.rs`.
Default Windows CI runs one production embedded-boundary smoke test; the manual
workflow runs all native security fixtures and a focused Rust runtime gate.
The manual Linux job runs the full workspace suite. Windows runtime tests run
serially because ACL recovery has a per-user lock. Focused check:
`cargo test -p latch-kernel --lib native_shell_and_fixed_git_use_embedded_boundary --locked`.
See `native/windows/boundary/README.md` for native fixture commands, verified
results, and known limitations.
The candidate helper has public-launcher and trusted-cleanup-owner roles;
`lifecycle.ps1` verifies launcher cancellation, tree termination and temporary
ACL/profile cleanup. `recovery.ps1` adds forced owner-kill and stale-state checks;
its CMake runner needs `-DLATCH_RECOVERY_TESTING=ON` (never enabled by Cargo).
The journal/identity protocol and remaining unsealed-profile blocker are in
`native/windows/boundary/RECOVERY.md`. Run overlapping native fixtures
sequentially: test-only journal overrides do not share the production lock.
Workspace grants include directory enumeration, traversal, metadata and file
reads. Ancestor handles remain metadata-only; protected journal and credential
subtrees remain inaccessible when a user's home is the workspace. The live
home-directory recovery attempt is unresolved; see
`native/windows/boundary/CURRENT_STATE.md` before more native testing.
Native ownership is split into runner/token/AppContainer/ACL/recovery/desktop/
job/process modules; Win32 complexity stays outside the safe Rust kernel.
TUI: `lib.rs` is app state/reducer plus `{transcript,markdown,chrome,
theme,runtime,agents,group,configuration_center}.rs` and existing siblings.

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
