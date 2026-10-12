# Code Review Findings

Point-in-time review of Latch v0.3.2 at commit `c2cf392` (2026-10-10), recorded
2026-10-12. Findings are code-reading results with the verification noted per
item. All 25 items were then processed one focused commit at a time; each
section carries its resolution and the summary table names the commit. CR-11
was verified to be a false positive and the review document was corrected
instead of changing code. Severity reflects likely impact, not frequency.

Baseline at review time:

- `cargo test --workspace --locked`: 828 passed, 0 failed (9 ignored).
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: clean.
- Script self-checks: `python3 scripts/test_install.py` 14/14, `node scripts/test_web_markdown.mjs` 5/5.

The problems below were therefore gaps not covered by the tests, lint, or
scripts present at review time rather than regressions in the suite.

## Summary

| ID | Severity | Area | Finding | Status |
|----|----------|------|---------|--------|
| CR-01 | High | kernel / tools | `git status -z` rename records are misparsed; undo can target a path outside the workspace | Fixed `f3f27cb` |
| CR-02 | High | kernel / safety | Approving an outside-workspace write to a top-level path mounts `/` writable in the sandbox | Fixed `3081b9b` |
| CR-03 | High | kernel / tools | Managed-process output is unbounded while running and `exec_poll` returns it unbounded | Fixed `074f13b` |
| CR-04 | Medium | kernel / provider | Responses API silently drops tool calls with unparseable arguments | Fixed `4220d2b` |
| CR-05 | Medium | kernel / approvals | Approval grants are keyed by the provider-supplied call id | Fixed `c3802c0` |
| CR-06 | Medium | kernel / agents | Worker shutdown busy-spins after lifetime cancellation | Fixed `785e417` |
| CR-07 | Medium | kernel / tools | Per-chunk `from_utf8_lossy` corrupts multi-byte output split across reads | Fixed `c4ad228` |
| CR-08 | Medium | kernel / media | Concurrent ingestion of the same image races on a fixed temp path | Fixed `ad99f1a` |
| CR-09 | Low-Med | kernel / provider | SSE frame decoder merges frames when line endings are mixed | Fixed `0d9ac80` |
| CR-10 | Low-Med | kernel / agent | `Agent::new` panics on recoverable supervisor/store errors | Fixed `3533bbb` |
| CR-11 | Low-Med | kernel / store | `EventStore` sets no `busy_timeout`; concurrent processes can hit `SQLITE_BUSY` | False positive — corrected `6a4263a` |
| CR-12 | Low | kernel / agents | `ensure_worker` check-then-spawn can orphan a duplicate worker | Fixed `24fffe1` |
| CR-13 | Low | kernel / agents | Group `reload` can clobber a concurrent in-memory update | Fixed `bda51de` |
| CR-14 | Medium | scripts | `fetch-references.sh` leaves a poisoned half-initialized checkout after a failed fetch | Fixed `3562437` |
| CR-15 | Low-Med | site | `site/build.py` rewrites links inside code blocks and does not protect tracked `docs/` | Fixed `7566b63` |
| CR-16 | Low-Med | benchmark | `compare.py` / `run_opencode.py` fail when `--output-dir` already exists | Fixed `fb6c0d9` |
| CR-17 | Low | scripts | `dogfood.sh` crashes on a flag missing its value; `--remove` is ignored with `--workspace` | Fixed `ef70132` |
| CR-18 | Low | scripts | `dogfood-test.sh` exits 1 for `KEEP=1` even when all checks pass | Fixed `3c33c9e` |
| CR-19 | Medium | CI | Windows `large-tree` recovery assertion never runs in CI; `cargo.ps1` is wired into no workflow | Fixed `8cf492d` |
| CR-20 | Medium | CI / supply chain | Workflows use mutable action tags, including the release publisher with `contents: write` | Fixed `c851404` |
| CR-21 | Low | docs | `AGENTS.md` references nonexistent "F1–F5" regressions and an incomplete snapshot-exception list | Fixed `f4add41` |
| CR-22 | Low | docs | `native/windows/boundary/README.md` still states "Version remains 0.2.3" | Fixed `1994180` |
| CR-23 | Low | docs | `docs/ARCHITECTURE.md` attributes a `group` module to `latch-tui`, which has none | Fixed `f10542d` |
| CR-24 | Low | docs | `docs/DESIGN.md` names a `/api/meta` endpoint; the real route is `/api/bootstrap` | Fixed `266ea90` |
| CR-25 | Low | docs / site | Sidebar width clamp is narrower than documented; the home page hardcodes `v0.3.2` | Fixed `1b5ea8a` |

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


**Resolution (`f3f27cb`):** Parsed `-z` output as a record stream: after an `R`/`C` status the following NUL field is consumed as the rename/copy source, so only destinations become worktree paths. Regression tests cover the exact `R  renamed.txt\0old.txt\0R  src/new.rs\0src/old.rs\0` byte sequence and real `git mv` output; both fail against the previous parser. Verified with a live repository before and after the change.

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


**Resolution (`3081b9b`):** `classify_file_write` and `classify_command` now deny a write whose derived approval root has no parent (a filesystem root) with an actionable message; the classification is a hard `Deny` that no grant can convert. Unit tests cover `write /x`, `echo x > /x`, and a Windows drive root (the Windows test is not executed on this host).

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


**Resolution (`074f13b`):** Managed output now lives in a `ProcessBuffer`: a bounded 4 MiB in-memory prefix with overflow streamed to the per-process artifact, and `exec_poll` returns at most 24 KB per response with the artifact reference. The durable exit event adopts the streaming artifact. Four `tools::process` tests cover the cap, poll bounding, artifact adoption, and retryable exit commits.

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


**Resolution (`4220d2b`):** `ResponsesStreamState::finish` returns a `Result`; an unparseable argument stream becomes a provider error naming the call id, function name, parse failure, and a bounded raw prefix, redacted at the transport like other provider errors. Regression test `responses_stream_invalid_tool_arguments_are_reported_not_dropped`.

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


**Resolution (`c3802c0`):** The kernel normalizes every model response so repeated provider ids get deterministic `#kernel-N` suffixes before the response is persisted or approved, and clears all single-use grants at both boundaries of `execute_batch`. Durable history, lifecycle events, and provider replay all use the normalized ids, so an approval applies to exactly one call. Two provider tests plus an end-to-end test where the second duplicate-id write must request its own (denied) approval.

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


**Resolution (`785e417`):** Extracted `race_turn` and disabled the lifetime-cancellation and command branches once closing starts, awaiting only the child turn; a closed command channel can no longer re-trigger the close branch either. Three tests pin late-command suppression after cancellation, interrupt handling, and closed-channel shutdown.

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


**Resolution (`c4ad228`):** Each output stream carries an incremental `Utf8Decoder`: incomplete trailing sequences are reassembled across reads and a genuinely truncated tail flushes as U+FFFD at EOF. Tests cover a split CJK character, invalid bytes, the EOF tail, and a reader integration that splits `é` across two writes.

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


**Resolution (`ad99f1a`):** Temp names are unique per attempt and a lost rename is accepted only after verifying the destination is the identical content-addressed artifact (also the Windows rename-over-existing case). An 8-thread concurrent-ingestion test plus direct `artifact_matches` checks.

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


**Resolution (`0d9ac80`):** The SSE decoder selects the earliest delimiter with its matching length instead of preferring `\n\n`. Two regression tests cover a CRLF frame followed by an LF frame and fragmented mixed-ending frames.

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


**Resolution (`3533bbb`):** Added `Agent::try_new`, which propagates supervisor reconstruction errors; the CLI build path uses it, and `new` remains a documented panicking test wrapper. A regression test injects an event-store append failure during child reconciliation and asserts a clean error.

### CR-11 — `EventStore` configures no `busy_timeout` — **false positive, corrected**

Location: `crates/latch-kernel/src/store.rs:160-179` (migrations set only
`journal_mode=WAL` and `foreign_keys=ON`).

This finding was based on the migration statements alone and missed the
connection default. `EventStore` opens its file connection with
`rusqlite::Connection::open` (`store.rs:130`) and its in-memory connection with
`Connection::open_in_memory` (`store.rs:144`). In `rusqlite 0.37.0`, every open
path (`Connection::open` -> `open_with_flags`, `open_in_memory` ->
`open_in_memory_with_flags` -> `open_with_flags`) calls
`InnerConnection::open_with_flags`, which already configures a 5000 ms SQLite
busy timeout:

```
// rusqlite-0.37.0/src/inner_connection.rs:119
let r = ffi::sqlite3_busy_timeout(db, 5000);
```

Concurrent writers in `WAL` mode therefore already retry within a bounded
5000 ms window; `PRAGMA busy_timeout` in the migration batch would be redundant.
No code change was made. If `rusqlite` ever changes that default, this
assumption must be revisited; the exact crate version is pinned by
`Cargo.lock` and `--locked` is used by every gate.


**Resolution (`6a4263a`):** False positive, corrected in the review document only. `rusqlite 0.37.0` sets `sqlite3_busy_timeout(db, 5000)` in every open path (`inner_connection.rs:119`), and `EventStore` uses `Connection::open`/`open_in_memory`, so concurrent writers already retry within five seconds. No code change was made.

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


**Resolution (`24fffe1`):** `ensure_worker` re-checks the worker map and inserts under the workers lock with the spawn inside the critical section, and worker creation is split into an unregistered `spawn_worker`. The graph lock is released before the workers lock, so the locks never nest in opposite orders. A four-task concurrent-ensure test asserts all callers share one worker channel and the map holds exactly one worker.

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


**Resolution (`bda51de`):** `reload` now holds the reducer lock from before the durable read through the assignment, so a concurrent `apply` runs after the rebuild. A test-only pause hook makes the interleaving deterministic; the regression test was run against the previous unlocked ordering and failed with the claimed `Pending` state before the fix.

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


**Resolution (`3562437`):** New checkouts are fetched into a unique staging directory and renamed into place only after verifying the exact shallow revision, with an EXIT trap cleaning staging on any failure. An existing destination without a valid HEAD is removed only when it is an empty git scaffold; anything that might hold work is refused with an actionable message. `scripts/test-fetch-references.sh` covers success, idempotence, poisoned-checkout recovery, refusal, and failed-fetch cleanup (6/6, local upstreams, no network) and runs in the Linux smoke job with shellcheck.

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


**Resolution (`7566b63`):** Qualified: Python-Markdown escapes quotes in fenced code blocks (`href=&quot;x&quot;`), so fenced samples were never affected. Indented code blocks, inline code spans, and raw HTML do preserve quotes and were affected. The rewrite now runs only outside `<pre>`/`<code>` regions, and the output guard rejects any in-repository output except the generated `target/` and `_site/` directories. `scripts/test_site_build.py` checks both, and the full build plus `scripts/test_site.py` pass unchanged (no current document is affected).

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


**Resolution (`fb6c0d9`):** Added a shared `run.report_root` helper: the requested output directory is treated as a container and a fresh stamped run directory is nested under it, matching `run.py`, and all three runners use it. `benchmark/test_runners.py` covers reuse of an existing directory, creation, naming, and permissions.

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


**Resolution (`ef70132`):** Every value-taking option requires a next argument and dies with a usage error (exit 2); `--provider-env` validates a shell-identifier name before indirect expansion; and `--workspace DIR --remove` now reports the run directory as not removed. The dogfood harness gains offline checks for all three.

### CR-18 — `dogfood-test.sh` reports failure when `KEEP=1`

Location: `scripts/dogfood-test.sh:353-361`.

`KEEP=1` is documented as "keep artifacts for inspection", but the summary
exit condition (`[ "$fail" -gt 0 ] || [ "${KEEP:-0}" = "1" ]`) returns exit 1
even when all checks pass, breaking wrappers that rely on status.

Suggested fix: exit 0 when `fail == 0`, keeping artifacts and printing where
they are; reserve exit 1 for real failures.


**Resolution (`3c33c9e`):** Summary policy moved into `dogfood_summary`: only real check failures exit 1, while `KEEP=1` keeps artifacts and exits 0. The harness checks the policy offline; the old condition exited 1 whenever `KEEP=1`.

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


**Resolution (`8cf492d`):** `large-tree` is now in the default `recovery.ps1` points so the bounded ancestor-handle assertion runs whenever the fixture runs, and the manual Full validation Windows native job invokes `cargo.ps1` through `VsDevCmd.bat` after building the fixtures, using the rustup sysroot. Workflow YAML was validated; the Windows fixtures themselves were not executed on this host.

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


**Resolution (`c851404`):** Every workflow action is pinned to its resolved 40-hex commit SHA with the release in a trailing comment, including the `contents: write` release publisher. `dtolnay/rust-toolchain` is pinned to its `stable` branch head and every workflow passes `toolchain: stable` explicitly, so the installed toolchain still tracks stable. `.github/dependabot.yml` requests weekly `github-actions` updates. All refs were verified against the upstream repositories and the YAML parses.

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


**Resolution (`f4add41`):** AGENTS.md now names the `f4_*` bounded-read tests instead of the nonexistent F1-F5 regressions and lists all four `include_str!` snapshots (`v31_diff`, `v31_sidebar`, `v3_semantic`, `v3_narrow`).

### CR-22 — Stale version in the Windows boundary README

Location: `native/windows/boundary/README.md:4` (and `:263-264`).

"Version remains 0.2.3" contradicts `Cargo.toml` (`0.3.2`) and the published
`docs/releases/0.3.*.md` entries. The file is linked from the root README as
current status, so the statement reads as live rather than historical.


**Resolution (`1994180`):** The Windows boundary README now records that its fixture results were taken against v0.2.3 and that tagged releases have been published since v0.3.0, instead of stating a live "Version remains 0.2.3" and that no release existed.

### CR-23 — `docs/ARCHITECTURE.md` attributes a `group` module to the TUI

Location: `docs/ARCHITECTURE.md:771-775`.

There is no `crates/latch-tui/src/group.rs` and no `mod group` in
`crates/latch-tui/src/lib.rs`. The group reducer lives in
`crates/latch-ui/src/group.rs`. The TUI module list should drop `group` (or the
sentence should point at the `latch-ui` module).


**Resolution (`f10542d`):** Removed `group` from the TUI module list in `docs/ARCHITECTURE.md`; the shared reducer lives in `crates/latch-ui/src/group.rs`, which the document already states elsewhere.

### CR-24 — `docs/DESIGN.md` names a nonexistent Web endpoint

Location: `docs/DESIGN.md:193-194`.

The router (`crates/latch-cli/src/web/http.rs:21-46`) exposes
`/api/bootstrap`; the frontend calls `/api/bootstrap`
(`web/app/app.js:60`). `/api/meta` occurs only in this document.


**Resolution (`266ea90`):** `docs/DESIGN.md` now names the real `/api/bootstrap` endpoint (router in `crates/latch-cli/src/web/http.rs`, frontend calls in `web/app/`); `/api/meta` existed only in the document.

### CR-25 — Small doc/site drift

- `docs/ARCHITECTURE.md:1121`: "clamped to 28–44 columns" holds only for
  terminals ≥160 columns; the per-breakpoint clamps in
  `crates/latch-tui/src/lib.rs:1669-1675` span 20–44.
- `site/templates/home.html:34`: hardcodes "implemented in v0.3.2" while
  `site/README.md` and `AGENTS.md` state version labels come from `Cargo.toml`;
  it will go stale on the next version bump.


**Resolution (`1b5ea8a`):** `docs/ARCHITECTURE.md` now describes the per-breakpoint sidebar clamp (20-44 columns) instead of only the >=160-column range, and `site/templates/home.html` uses the build's `v$version` substitution instead of hardcoded `v0.3.2`; a rebuilt home page renders the Cargo.toml version.

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

## Final validation

Every item above is one focused commit on top of `c2cf392` (the resolution
lines and the summary table name the commits). Validation on the final tree:

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: clean.
- `cargo test --workspace --locked`: 873 passed, 0 failed, 9 ignored (the
  ignored tests are the opt-in live-provider suites).
- `bash scripts/release-gate.sh`: passed, including the mandatory Bubblewrap
  namespace/mount probe, the architectural invariants (20/20), the full
  workspace suite, and the locked release build.
- Scripts and tooling: `python3 scripts/test_install.py` 14/14,
  `bash scripts/test-fetch-references.sh` 6/6,
  `shellcheck scripts/install.sh scripts/fetch-references.sh scripts/test-fetch-references.sh`
  clean (0.10.0), `node scripts/test_web_markdown.mjs` 5/5,
  `python3 benchmark/verify_cases.py` verified all 25 candidate cases,
  `python3 benchmark/test_runners.py` 4/4.
- Site: `python3 site/build.py` plus `scripts/test_site.py` (26 HTML pages,
  1468 local links/assets/fragments, 208 search sections) and
  `scripts/test_site_build.py` passed with Python-Markdown 3.7.

Not executed on this host and therefore not claimed: the Windows native
fixtures and Windows CI lanes (no Windows host; the workflow wiring was
YAML-validated only), the optional Chromium browser checks (no Chromium), the
Docker dogfood harness (its offline argument and summary checks were run
directly), and the opt-in live provider tests.
