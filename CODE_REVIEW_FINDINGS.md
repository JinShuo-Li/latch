# Code Review Findings

Point-in-time review of Latch v0.3.2 at commit `c2cf392` (2026-10-10), recorded
2026-10-12. Findings are code-reading results with the verification noted per
item; none have been fixed yet. Severity reflects likely impact, not frequency.

Baseline at review time:

- `cargo test --workspace --locked`: 828 passed, 0 failed (9 ignored).
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: clean.
- Script self-checks: `python3 scripts/test_install.py` 14/14, `node scripts/test_web_markdown.mjs` 5/5.

The problems below are therefore gaps not covered by the current tests, lint, or
scripts rather than regressions in the suite.

## Summary

| ID | Severity | Area | Finding | Status |
|----|----------|------|---------|--------|
| CR-01 | High | kernel / tools | `git status -z` rename records are misparsed; undo can target a path outside the workspace | Open |
| CR-02 | High | kernel / safety | Approving an outside-workspace write to a top-level path mounts `/` writable in the sandbox | Open |
| CR-03 | High | kernel / tools | Managed-process output is unbounded while running and `exec_poll` returns it unbounded | Open |
| CR-04 | Medium | kernel / provider | Responses API silently drops tool calls with unparseable arguments | Open |
| CR-05 | Medium | kernel / approvals | Approval grants are keyed by the provider-supplied call id | Open |
| CR-06 | Medium | kernel / agents | Worker shutdown busy-spins after lifetime cancellation | Open |
| CR-07 | Medium | kernel / tools | Per-chunk `from_utf8_lossy` corrupts multi-byte output split across reads | Open |
| CR-08 | Medium | kernel / media | Concurrent ingestion of the same image races on a fixed temp path | Open |
| CR-09 | Low-Med | kernel / provider | SSE frame decoder merges frames when line endings are mixed | Open |
| CR-10 | Low-Med | kernel / agent | `Agent::new` panics on recoverable supervisor/store errors | Open |
| CR-11 | Low-Med | kernel / store | `EventStore` sets no `busy_timeout`; concurrent processes can hit `SQLITE_BUSY` | Open |
| CR-12 | Low | kernel / agents | `ensure_worker` check-then-spawn can orphan a duplicate worker | Open |
| CR-13 | Low | kernel / agents | Group `reload` can clobber a concurrent in-memory update | Open |
| CR-14 | Medium | scripts | `fetch-references.sh` leaves a poisoned half-initialized checkout after a failed fetch | Open |
| CR-15 | Low-Med | site | `site/build.py` rewrites links inside code blocks and does not protect tracked `docs/` | Open |
| CR-16 | Low-Med | benchmark | `compare.py` / `run_opencode.py` fail when `--output-dir` already exists | Open |
| CR-17 | Low | scripts | `dogfood.sh` crashes on a flag missing its value; `--remove` is ignored with `--workspace` | Open |
| CR-18 | Low | scripts | `dogfood-test.sh` exits 1 for `KEEP=1` even when all checks pass | Open |
| CR-19 | Medium | CI | Windows `large-tree` recovery assertion never runs in CI; `cargo.ps1` is wired into no workflow | Open |
| CR-20 | Medium | CI / supply chain | Workflows use mutable action tags, including the release publisher with `contents: write` | Open |
| CR-21 | Low | docs | `AGENTS.md` references nonexistent "F1–F5" regressions and an incomplete snapshot-exception list | Open |
| CR-22 | Low | docs | `native/windows/boundary/README.md` still states "Version remains 0.2.3" | Open |
| CR-23 | Low | docs | `docs/ARCHITECTURE.md` attributes a `group` module to `latch-tui`, which has none | Open |
| CR-24 | Low | docs | `docs/DESIGN.md` names a `/api/meta` endpoint; the real route is `/api/bootstrap` | Open |
| CR-25 | Low | docs / site | Sidebar width clamp is narrower than documented; the home page hardcodes `v0.3.2` | Open |

## 1. Correctness and security

### CR-01 — `git status --porcelain -z` rename records are misparsed

Location: `crates/latch-kernel/src/tools/ownership.rs:538-566` (`git_porcelain`),
used by `git_dirty_hashes` (`:521-536`) and `classify_drift` (`:226-388`).

In `-z` format, a rename entry is `XY <destination>` followed by a separate
NUL-terminated `<source>` field (`git help status`, "the field order is
reversed"). Verified against a real repository:

```
R  renamed.txt\0top.txt\0R  src/new.rs\0src/old.rs\0
```

The parser treats every NUL field as a fresh `XY path` line and strips three
bytes unconditionally (`ownership.rs:555-562`), so the source fields become
bogus paths:

- `top.txt` -> `<workspace>/p.txt`
- `src/old.rs` -> `/old.rs` (the stripped result is absolute, and
  `Path::join` replaces the base, escaping the workspace)

Impact chain: `classify_drift` turns these into change candidates; `relative()`
deliberately returns outside-workspace paths verbatim (`tools.rs:664-672`), so a
candidate that exists on disk can be recorded in the undo ledger as
`created: true` with no captured content. A later `/undo` then executes
`tokio::fs::remove_file(path)` (`ownership.rs:444-452`) against that path — for
`/old.rs`, a host file outside the workspace. Even when the file does not
exist, phantom paths pollute drift reports and `git_dirty_hashes`.
The `split_once(" -> ")` branch is dead code under `-z`.

Suggested fix: parse `-z` records as records, not line fragments: after an `R`
or `C` status, consume the following field as the source path and keep only the
destination (field one) as the worktree path. Add a regression test built from
the byte sequence above.

### CR-02 — Outside-workspace write approval can grant a writable mount of `/`

Location: `crates/latch-kernel/src/safety.rs:213-222` (`classify_file_write`),
`:305-317` (`classify_command`), `crates/latch-kernel/src/tools.rs:257-268`
(profile merge), `crates/latch-kernel/src/sandbox.rs:341-345` (mount).

For a write target outside the workspace, the approval root is
`path.parent()`. For a top-level path such as `/newfile` (via the `write` tool
or `echo x > /newfile`), the parent is `/`, and `system_destructive_target`
(`safety.rs:708-764`) does not reject ordinary top-level paths. After the user
(or auto-approval / AI review) approves, `SandboxProfile::with_external_roots`
carries `/`, and the Bubblewrap command executes `--bind / /`: the approved
command runs with the entire host filesystem mounted writable, far beyond the
single file named in the request. The Windows runtime grants the same root
through its ACL path. Secret masks and the state directory remain protected,
but arbitrary user data outside those paths becomes writable.

Suggested fix: refuse approval roots that are filesystem roots (or equally
broad), or fall back to binding only the named file; at minimum require an
explicit second confirmation that names the directory being granted. A unit
test asserting that `write /x` does not produce a `/` root would pin this.

### CR-03 — Managed-process output is unbounded

Location: `crates/latch-kernel/src/tools/process.rs:414-456` (`refresh_process`),
`:458-477` (`PROCESS_MAX_BUFFER_BYTES`, `spawn_reader`),
`crates/latch-kernel/src/tools.rs:501-529` (`execute` bounding).

`refresh_process` returns early for a still-running process (`:428-430`), so the
4 MiB spill to an artifact only runs after exit. Meanwhile `spawn_reader`
appends every chunk to a `String` with no cap, and `process_poll` returns the
entire unread remainder as the tool output (`process.rs:140-157`).
`ToolExecutor::execute` bounds output only for `read_file` and `read_artifact`
(`tools.rs:523-529`). A chatty process (`yes`, a noisy dev server) grows
unbounded memory for the session lifetime and can inject an arbitrarily large
`ToolResult` into the durable event log and the next model request in one poll.

Suggested fix: enforce the cap inside `spawn_reader` (retain a bounded prefix or
tail and stream overflow to the artifact file), and bound `exec_poll` output
with an artifact reference for the remainder, consistent with the large-output
convention used by other tools.

## 2. Robustness and races

### CR-04 — Responses API silently drops tool calls with unparseable arguments

Location: `crates/latch-kernel/src/provider.rs:824-850`
(`filter_map` with `serde_json::from_str(&args).ok()?` at `:829-834`).

A truncated or malformed `function_call` argument stream removes the call
entirely. The response degrades to text-only, and the agent loop can finish the
run as if the model had answered without tools, instead of reporting a bad
provider response. The Chat Completions adapter treats the same condition as a
hard error with context (`provider.rs:515-525`), so behavior is inconsistent
across transports.

Suggested fix: return an error carrying the call id and raw argument prefix, as
the Chat Completions adapter does. Add a provider-serialization unit test.

### CR-05 — Approval grants are keyed by the provider-supplied call id

Location: `crates/latch-kernel/src/agent/dispatch.rs:74-91, 112-135`,
`crates/latch-kernel/src/tools.rs:440-449`, id parsing in
`crates/latch-kernel/src/provider.rs:499-511` (OpenAI), Anthropic/Gemini block
ids elsewhere in the same file.

The comment at `dispatch.rs:74-76` states the approval is "keyed to this kernel
call id (which the model never supplies)", but `ToolCall.id` is parsed directly
from provider wire data. If one batch carries two calls with the same id (a
buggy or hostile endpoint), the first `Ask` stores a grant under that id and the
second call takes the `has_grant` fast path, skipping its own classification and
approval while reusing the first grant's capabilities and `external_roots`.

Suggested fix: derive a kernel-owned per-call nonce for approval binding
(e.g. a UUID or sequence stored alongside the call), and use the provider id
only for serialization.

### CR-06 — Worker shutdown busy-spins after lifetime cancellation

Location: `crates/latch-kernel/src/agents/worker.rs:214-235`.

When `lifetime.cancelled()` fires, the select branch sets `closing`, cancels
`turn_cancel`, and loops. The cancellation future is then permanently ready, so
the loop re-enters `select!` immediately and spins at full CPU until `agent.run`
observes cancellation and completes (which can take seconds while a child
process is reaped).

Suggested fix: after the first cancellation, set the flag and await only
`&mut run` (or break and run the closing path explicitly). A bounded assertion
on CPU behavior is hard to test; a code-level test can at least pin that the
branch does not re-arm.

### CR-07 — Per-chunk UTF-8 decoding corrupts split multi-byte characters

Location: `crates/latch-kernel/src/tools/process.rs:461-477`.

`spawn_reader` calls `String::from_utf8_lossy(&buffer[..n])` on each 4096-byte
read, so a multi-byte character split across a read boundary becomes two U+FFFD
replacement characters. CJK and emoji output from `exec_start` is silently
corrupted; `run_process_inner` avoids this by collecting bytes and decoding once
(`process.rs:289-320`).

Suggested fix: keep a byte buffer (or a small carry for incomplete sequences)
and decode on complete boundaries, matching `run_process_inner`; emit U+FFFD
only for genuinely invalid data.

### CR-08 — Concurrent identical image ingestion races on a fixed temp path

Location: `crates/latch-kernel/src/media.rs:336-350`.

The temp filename is deterministic: `media/.{sha256}.{pid}.tmp`. Two
concurrent ingestions of the same image in one process — for example two
`read_image` calls in one read-only tool batch, which `dispatch.rs:168-177`
executes concurrently — both see `path.exists() == false`, write the same temp
file, and race: one `rename` wins while the loser fails (spurious tool error),
or the second writer keeps writing through the renamed inode and corrupts the
content-addressed artifact. On Windows the same destination may already exist,
where `rename` cannot replace it.

Suggested fix: use a unique temp name per attempt (random suffix) and treat a
lost rename as success when the destination exists and hashes match, or use
`create_new` and retry. Add a test that ingests one image twice concurrently.

### CR-09 — SSE decoder can merge two frames when line endings are mixed

Location: `crates/latch-kernel/src/provider.rs:2709-2727`.

The delimiter match arms prefer the `\n\n` position even when a
`\r\n\r\n` delimiter occurs earlier:

```rust
(Some(a), Some(b)) if a <= b => Some((a, 2)),
(Some(a), _)             => Some((a, 2)),   // also fires when b < a
```

A buffer with a `\r\n\r\n`-terminated frame followed by an `\n\n`-terminated one
(possible behind proxies that normalize line endings) is consumed to the later
position, merging both `data:` payloads and producing a confusing JSON parse
error.

Suggested fix: select `min(a, b)` and use the corresponding delimiter length:
`(Some(a), Some(b)) if a < b => (a,2)`, `(Some(_), Some(b)) => (b,4)`, etc.

### CR-10 — `Agent::new` panics on recoverable supervisor construction errors

Location: `crates/latch-kernel/src/agent.rs:195-204`
(`AgentSupervisor::new(...).expect("reconstruct durable agent graph")`).

`AgentSupervisor::new` performs several fallible operations (graph reload,
reconciliatory append, group coordinator reload). An I/O error, locked or
corrupt database, or poisoned group state turns into a panic in the CLI's
`build_agent` path (`crates/latch-cli/src/cli/session.rs:676`) even though the
caller already returns `Result` and can surface a clean error.

Suggested fix: add a fallible constructor (`try_new`) and propagate the error;
keep `new` as a test-only wrapper or remove the `expect`.

### CR-11 — `EventStore` configures no `busy_timeout`

Location: `crates/latch-kernel/src/store.rs:160-179` (migrations set only
`journal_mode=WAL` and `foreign_keys=ON`).

Two Latch processes sharing one state directory (for example a running TUI plus
`latch sessions` or `latch run`) can hit `SQLITE_BUSY` immediately on write
contention instead of waiting, producing user-visible "database is locked"
failures.

Suggested fix: set `PRAGMA busy_timeout = <ms>` (for example 5000) during
`migrate`/`open` so concurrent writers retry within a bounded window.

### CR-12 — `ensure_worker` check-then-spawn can orphan a worker

Location: `crates/latch-kernel/src/agents/supervisor.rs:612-668`
(`ensure_worker` -> `start_worker`).

`ensure_worker` checks the worker map, then `start_worker` spawns the task and
inserts the handle, with no lock spanning both. Two concurrent callers (for
example root sending a message while a worker is being rebuilt after resume)
can both observe `None`, spawn two workers, and let the second `insert`
silently overwrite the first handle. The orphaned worker runs with no sender
until the supervisor drops; durable state is unaffected but a task and child
`Agent` leak.

Suggested fix: reserve the slot atomically before spawning (insert a placeholder
or hold the map lock across the spawn decision), or make insertion detect the
race and shut the loser down.

### CR-13 — Group `reload` can clobber a concurrent in-memory update

Location: `crates/latch-kernel/src/agents/group.rs:502-537`.

`reload` reads all durable group events without holding the state mutex, then
assigns `*self.state() = state` (`:531`). A concurrent `apply` that commits
between the read and the assignment is lost from the in-memory reducer until
the next reload, so `snapshot`/`status` can briefly report stale ownership.
The store remains authoritative, so claims still work.

Suggested fix: hold the state lock across the rebuild (reading events inside the
critical section), or reapply events newer than the snapshot sequence after
assignment.

## 3. Tooling, scripts, and CI

### CR-14 — `fetch-references.sh` permanently poisons itself after a failed fetch

Location: `scripts/fetch-references.sh:13-31`.

If `git fetch` (`:27`) fails after `git init` (`:25`), the destination exists
with no `HEAD`. Every later run takes the `[[ -e "$destination" ]]` branch and
dies at `git -C "$destination" rev-parse HEAD` (`:14`) under `set -e` with a
confusing "ambiguous argument 'HEAD'" error, instead of the intended "refusing
to replace existing reference checkout" message or a recovery path. Manual
`rm -rf .references/<name>` is the only fix, and nothing tells the user.

Suggested fix: treat a checkout with no valid `HEAD` as incomplete — remove it
and retry, or fetch into a temporary directory and rename into place only after
verification. Print a recovery hint on failure.

### CR-15 — `site/build.py` rewrites links inside code blocks and can write into tracked `docs/`

Location: `site/build.py:60-65` (output guard), `:116-120` (link rewrite).

The rewrite regex runs over the converted HTML, so `href=`/`src=` occurrences
inside `<pre><code>` samples are rewritten too. Adding a documentation code
sample containing `href="x"`/`src="x"` either aborts the Pages build with
`Missing repository reference` (`:105-106`) or silently rewrites the displayed
sample. Separately, the output guard rejects only the repository root and
`site/`; `python3 site/build.py --output docs` succeeds and scatters generated
files (`index.html`, `app.js`, `styles.css`, `assets/`, `captures/`, installer
copies) into the tracked `docs/` tree.

Suggested fix: restrict the rewrite to text outside `code`/`pre` regions (for
example by splitting on code blocks or rewriting before conversion), and extend
the output guard to reject any path inside a tracked repository directory.

### CR-16 — Benchmark runners fail when `--output-dir` already exists

Location: `benchmark/compare.py:91-93`, `benchmark/run_opencode.py:153-155`.

When `--output-dir` is omitted, both tools stamp a unique subdirectory; when it
is supplied, `root.mkdir(parents=True, mode=0o700)` (no `exist_ok`) raises
`FileExistsError` for an existing directory. Re-running a comparison into a
mounted results directory cannot work. `benchmark/run.py:303-306` always nests a
fresh run directory and behaves consistently.

Suggested fix: mirror `run.py` — always nest a stamped/pid subdirectory under
the requested output root, or use `exist_ok=True` plus a collision-resistant
stamp.

### CR-17 — `dogfood.sh` argument handling edge cases

Location: `scripts/dogfood.sh:87-111` (flag loop), `:301-306` (removal).

- Every value-taking flag reads `$2` unconditionally under `set -u`;
  `./scripts/dogfood.sh --prompt` fails with `$2: unbound variable` and exit 1
  instead of the intended usage error (exit 2).
- `--provider-env <name>` forwards `${!name:-}`, which errors for names that are
  not valid shell identifiers.
- `--workspace <custom> --remove` is silently ignored because removal requires
  `disposable=1`, and neither "removed" nor "kept" is printed.

Suggested fix: validate that each value flag has a next argument, validate
`--provider-env` names, and warn when `--remove` cannot act.

### CR-18 — `dogfood-test.sh` reports failure when `KEEP=1`

Location: `scripts/dogfood-test.sh:353-361`.

`KEEP=1` is documented as "keep artifacts for inspection", but the summary
exit condition (`[ "$fail" -gt 0 ] || [ "${KEEP:-0}" = "1" ]`) returns exit 1
even when all checks pass, breaking wrappers that rely on status.

Suggested fix: exit 0 when `fail == 0`, keeping artifacts and printing where
they are; reserve exit 1 for real failures.

### CR-19 — CI-native coverage gaps on Windows

Location: `.github/workflows/full-validation.yml:71-94`,
`native/windows/boundary/recovery.ps1:15-16, 23-49`,
`native/windows/boundary/RECOVERY.md:183,204`.

- `large-tree` is special-cased in `recovery.ps1` but is not in the default
  `$points` list and can only be enabled via `LATCH_RECOVERY_POINTS`; no
  workflow, script, or document ever sets that variable, so the bounded
  ancestor-handle assertion at `recovery.ps1:45-48` never executes in CI.
- `native/windows/boundary/cargo.ps1` (sandboxed Cargo compile/link/unit/rustdoc
  and cleanup) is referenced only by `RECOVERY.md`; no workflow invokes it, so
  the sandboxed-Cargo path has no automated coverage.

Suggested fix: add `large-tree` to the default points (or set
`LATCH_RECOVERY_POINTS` in the workflow), and wire `cargo.ps1` into the native
job or explicitly document why it stays manual.

### CR-20 — Workflows use mutable action tags, including the release publisher

Location: `.github/workflows/*.yml` (`uses:` entries), notably
`.github/workflows/release.yml:143-166` (`publish` has `contents: write` and
uses `actions/download-artifact@v8`; build jobs use
`actions/checkout@v7`, `dtolnay/rust-toolchain@stable`,
`Swatinem/rust-cache@v2`, `actions/upload-artifact@v7`).

A compromised or retagged action reference can run arbitrary code in jobs that
hold `GH_TOKEN` and `contents: write`, tampering with published binaries and
`SHA256SUMS`. There is no Dependabot configuration or pinning policy in
`.github/`.

Suggested fix: pin third-party actions to commit SHAs (with a comment naming
the release) and enable Dependabot updates; keep `stable` only where a moving
major is explicitly intended.

## 4. Documentation

### CR-21 — `AGENTS.md` inaccuracies

Location: `AGENTS.md:53`, `AGENTS.md:116-118`.

- "The full workspace suite covers deterministic F1–F5 regressions" — `F1`–`F5`
  appears nowhere else in the repository; the only related identifiers are two
  tests named `f4_...` in `crates/latch-kernel/src/tools/tests.rs:1688,1806`.
- The snapshot-exception list names two files, but four snapshots are loaded
  with `include_str!` and cannot be refreshed by `LATCH_UPDATE_SNAPSHOTS=1`:
  `v31_diff` (`crates/latch-tui/src/diff.rs:346`), `v31_sidebar`
  (`sidebar.rs:2000`), `v3_semantic` and `v3_narrow`
  (`presentation.rs:193,235`).

### CR-22 — Stale version in the Windows boundary README

Location: `native/windows/boundary/README.md:4` (and `:263-264`).

"Version remains 0.2.3" contradicts `Cargo.toml` (`0.3.2`) and the published
`docs/releases/0.3.*.md` entries. The file is linked from the root README as
current status, so the statement reads as live rather than historical.

### CR-23 — `docs/ARCHITECTURE.md` attributes a `group` module to the TUI

Location: `docs/ARCHITECTURE.md:771-775`.

There is no `crates/latch-tui/src/group.rs` and no `mod group` in
`crates/latch-tui/src/lib.rs`. The group reducer lives in
`crates/latch-ui/src/group.rs`. The TUI module list should drop `group` (or the
sentence should point at the `latch-ui` module).

### CR-24 — `docs/DESIGN.md` names a nonexistent Web endpoint

Location: `docs/DESIGN.md:193-194`.

The router (`crates/latch-cli/src/web/http.rs:21-46`) exposes
`/api/bootstrap`; the frontend calls `/api/bootstrap`
(`web/app/app.js:60`). `/api/meta` occurs only in this document.

### CR-25 — Small doc/site drift

- `docs/ARCHITECTURE.md:1121`: "clamped to 28–44 columns" holds only for
  terminals ≥160 columns; the per-breakpoint clamps in
  `crates/latch-tui/src/lib.rs:1669-1675` span 20–44.
- `site/templates/home.html:34`: hardcodes "implemented in v0.3.2" while
  `site/README.md` and `AGENTS.md` state version labels come from `Cargo.toml`;
  it will go stale on the next version bump.

## Suggested repair order

1. CR-01 (possible out-of-workspace deletion), CR-02 (sandbox privilege
   scope), CR-03 (unbounded memory/output), CR-05 (approval bypass).
2. CR-04, CR-06, CR-07, CR-08, CR-09.
3. CR-14 through CR-20 (tooling and CI coverage).
4. CR-10 through CR-13 and the documentation items.

Each item is independently fixable and can carry a focused regression test in
the existing suites (`tools/tests.rs` for drift/undo and process handling,
`provider.rs` unit tests for SSE and Responses parsing, `media.rs` tests for
concurrent ingestion, and the shell/Python checks for the script and site
issues).
