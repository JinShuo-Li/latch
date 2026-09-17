# Long-horizon and real-provider test report

Empirical report on how the Latch agent behaves on long-horizon tasks, measured
in two independent environments: the isolated Docker dogfood harness and a
real-provider live-acceptance run on the host through the mandatory Bubblewrap
sandbox. Every number below was measured on the run described; nothing is
extrapolated.

| Field | Value |
| --- | --- |
| Date | 2026-09-17 |
| Source revision | `51e92f7` (`docs(architecture): align prompt cache layout with the session block`), branch `dev`, clean tree |
| Container image | `latch-dogfood:longhorizon`, revision label `51e92f706848e928353169ae717e024535dd258c` (`provenance_ok=true`) |
| Host | Raspberry Pi 5 Model B Rev 1.1, aarch64, 4 cores, 4 GiB RAM, Debian 13 (trixie), glibc 2.41 |
| Docker / bwrap | Docker Engine 29.8.0 (BuildKit v0.33.0); host bubblewrap 0.12.0, container bubblewrap 0.8.0 |
| Provider / model | OpenCode Go, `deepseek-v4-flash`, effort `provider_default` |
| Machine interface | `latch run --output json`, mode `WORK`, safety `standard`, permissions `human` |

## 1. Objective and scope

Answer three questions with observed evidence:

1. Can the agent sustain a long horizon (many model turns, many tool calls,
   repeated validation) without losing the thread or tripping a supervisor?
2. Does long-horizon correctness survive context rollover and a resume?
3. Does it all hold inside the Docker isolation boundary with the inner
   Bubblewrap sandbox unchanged?

This is a **measurement** exercise. It does not change provider behavior,
kernel semantics, or any product code. It does not propose fixes.

## 2. Safety controls

No file in the repository, the user's home, or the user's Latch state was
modified by a run. Safety was enforced by construction, not by review:

- **Disposable workspaces.** Every Docker run used a freshly copied fixture
  under `target/dogfood/<run-id>/workspace` (git-ignored). The live scenarios
  used `tempfile::tempdir()`.
- **Docker hardening.** The runner never used `--privileged`, `--network host`,
  the Docker socket, host PID/IPC namespaces, or host credential mounts. It
  applied `--rm`, `--cap-drop ALL`, `--security-opt no-new-privileges:true`,
  `--security-opt seccomp=unconfined`, `--security-opt systempaths=unconfined`,
  `--pids-limit 512`, `--user <host uid:gid>`, and mounted only the workspace,
  the run-scoped state, the run-scoped home, and a read-only config. The two
  `seccomp`/`systempaths` exceptions are the documented compatibility baseline
  required for nested unprivileged user namespaces; they grant no extra
  capability.
- **Inner sandbox unchanged.** Every tool command still ran under Latch's
  mandatory Bubblewrap sandbox (read-only host root, private `/tmp`, masked
  `~/.ssh`/GnuPG/cloud credentials, no network unless granted). The agent's
  provider traffic is made by the Latch process, not by sandboxed commands.
- **Credentials.** Only `OPENCODE_API_KEY` was forwarded, by name, per run; it
  was never written to the image, the config, or the event log.
- **Isolated user state.** Live scenarios pinned `XDG_CONFIG_HOME` and the
  config's `state_dir` to `/tmp/opencode/latch-test/...`; no `~/.config/latch`
  or `~/.local/state/latch` existed or was created.
- **Non-destructive image work.** The pre-existing `latch-dogfood:local` image
  was left untouched; a new tag, `latch-dogfood:longhorizon`, was built.

## 3. Environment note: rebuilding the image without Docker Hub

Docker Hub was unreachable from this host (`registry-1.docker.io` refused both
IPv4 and IPv6; even `docker manifest inspect rust:1-bookworm` failed). The
existing `latch-dogfood:local` image is stale relative to `HEAD` and carries no
provenance label, so it was not used for evidence.

Instead the image was rebuilt from the current source offline, using base
images that were already present in the local BuildKit cache and referenced by
digest:

```sh
docker build --target runtime \
  -f /tmp/opencode/latch-test/Dockerfile.longhorizon \
  -t latch-dogfood:longhorizon \
  --build-arg LATCH_SOURCE_REVISION=$(git rev-parse HEAD) .
```

The build file is byte-for-byte the repository `docker/Dockerfile` runtime
stage except that `FROM rust:1-bookworm` is pinned to
`@sha256:9a73a5088750b4c95158ab26629c854c3d6fc4b173cb7bc8079ad252d8ed7bfa` and
the `# syntax` directive is removed (the built-in frontend supports
`--mount=type=cache`). Runtime packages (`bubblewrap`, `ripgrep`, `git`,
`python3`, `ca-certificates`) still came from `deb.debian.org`. The resulting
image label equals `git rev-parse HEAD`, and `provenance.json` recorded
`provenance_ok=true` for every run. The binary reports `latch 0.2.1`.

## 4. Method

### 4.1 Docker long-horizon fixture (`taskqueue`)

A dependency-free Python package with a `README.md`, four modules, and a
`unittest` suite (15 tests, 9 initially failing):

- `models.py` — `Task` dataclass and `Priority` enum.
- `store.py` — in-memory store with duplicate detection and pending filtering.
- `parser.py` — one-line DSL parser (tags and priority markers).
- `scheduler.py` — highest-priority selection and capacity reporting.

Bugs were planted in all four modules; the tests are the specification and must
not be modified. The single prompt was:

> Read README.md carefully. The unittest suite under tests/ currently fails.
> Fix the implementation under taskqueue/ so that every test passes. Do not
> modify or add anything under tests/. Run `python3 -m unittest discover -s
> tests -v` to verify, and keep going until the suite is green.

Correctness was verified independently after each run by re-running the suite
from the host in the preserved workspace. This fixture exercises multi-file
comprehension, hash-guarded edits, and a validation gate, but produces only
8–10 model turns per run; it is a Docker packaging test, not the turn-count
stress. Two window settings were used to compare normal and
rollover-inducing behavior.

### 4.2 Live long-horizon scenarios

`crates/latch-kernel/tests/live_acceptance.rs` runs against the real provider
with real policy, tools, and validation, in a `tempdir` workspace under
Bubblewrap:

- `long` — 40 sequential user requests to append notes to a Rust file, each
  typically 2–4 model turns; success requires >100 model turns. Run twice: once
  at the model's declared 1,048,576-token window and once with the window
  overridden to 32,768 tokens to force context pressure.
- `interrupt` — cancel a run mid-flight, then resume the same durable session
  and finish.

Invocations:

```sh
XDG_CONFIG_HOME=/tmp/opencode/latch-test/live LATCH_LIVE_TESTS=1 \
  LATCH_LIVE_SCENARIO=long cargo test -p latch-kernel --test live_acceptance \
  -- --ignored --nocapture
```

The reduced-window variant used `XDG_CONFIG_HOME=/tmp/opencode/latch-test/live-small`
with `[providers.opencode-go.models."deepseek-v4-flash"] context_window_tokens = 32768`.

## 5. Results

### 5.1 Docker real-provider runs

Model `deepseek-v4-flash`; all runs exited `0` with `completion=verified` and
`tests 15/15 OK`.

| Run | Window | Model turns | Tool calls | Self-heal failures | Validation | Wall clock | Input tok | Output tok | Cache read | Cache miss | read/input |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `lh-taskqueue-01` | 1,048,576 | 10 | 21 | 2 | 1 pass / 0 fail | 70 s | 102,192 | 3,064 | 79,104 | 12,661 | 0.774 |
| `lh-taskqueue-small` | 24,000 | 8 | 19 | 1 | 1 pass / 0 fail | 51 s | 69,654 | 3,253 | 56,064 | 13,590 | 0.805 |

Tool mix (`lh-taskqueue-01`): `read_file` ×11, `patch` ×6, `shell` ×1,
`validate` ×1, `task_update` ×1, `complete` ×1. All 18 permission decisions
were `Allow`; there were no unresolved asks and no denials.

Per-turn input / cache-read for `lh-taskqueue-01` shows the expected cold start
and warm-up:

```
turn  0   1    2     3     4      5      6      7      8      9
in  4685 6296 7485  8964 10427 11712 12024 12495 13618 14486
rd     0 4736 6400  7552     0 10496 11776 12288 12288 13568
```

Turn 4 reported `input=10427` with `cache_read=0` **and** `cache_miss=0`: the
provider returned no cache fields for that response. The aggregate is therefore
a slight underestimate of true cache reads and is treated as a lower bound.

### 5.2 Host live long-horizon runs

Real provider, real tools, Bubblewrap sandbox, `tempdir` workspace.

| Scenario | Window | Model turns | Tool calls | Validation | Wall clock | Input tok | Output tok | Cache read | read/input | Peak context | Completion |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| `long` | 1,048,576 | 161 | 144 | 40 pass / 0 fail | 500.3 s | 3,867,673 | 26,958 | 3,517,440 | 0.909 | 50,787 | Verified |
| `long` | 32,768 | 189 | 176 | 40 pass / 0 fail | 638.4 s | 3,968,060 | 29,079 | 3,638,784 | 0.917 | 47,360 | Verified |
| `interrupt` | 1,048,576 | 6 | 7 | 1 pass / 0 fail | 12.0 s | 28,586 | 594 | 26,496 | 0.927 | 7,569 | Verified |

Both `long` runs sustained far beyond the >100-turn target (161 and 189 turns,
~4 turns per user request) with **zero** validation failures and **zero**
unresolved permission denials. The 32,768-token window did not reduce
completion: the run actually made more turns (189) and still verified, and its
`estimated_request_context_tokens` high-water (47,360) reflects the pre-rotation
estimate, not the sent request.

### 5.3 Context rollover, measured through the persisted CLI event log

The live harness keeps events in memory, so rollover is not observable there.
The CLI/Docker path persists the durable event log, and the reduced-window run
`lh-taskqueue-small` rotated exactly once, with the correctness outcome
unchanged:

```
seq 113  epoch=0  epoch_tokens=8353  recent_evicted=0     reason=''
seq 129  epoch=1  epoch_tokens=6552  recent_evicted=3755  reason='working budget high-water mark reached'  retained=5751  episodes=3
seq 140  epoch=1  epoch_tokens=9429  recent_evicted=0     reason='working budget high-water mark reached'  retained=5751  episodes=3
```

- Rotation was whole-unit and hysteretic: one rotation, not per-turn.
- The raw event log was preserved (no lossy compaction); three archival
  episodes were created and remained available.
- The first request after rotation (`input=9628`, `cache_read=4096`) shows the
  expected prefix re-warm; the next request recovered (`input=11733`,
  `cache_read=9728`).
- The final request used 12,991 of the 24,000-token window, and the 15 tests
  still passed.

### 5.4 Deterministic continuity stress

Run alongside the live work as the fast invariant tier:

```sh
cargo test -p latch-kernel --lib continuity -- --nocapture
```

13 tests passed, including `cache_epochs_append_between_rotations_and_rotate_with_hysteresis`,
`materialized_context_is_bounded_under_thousands_of_events`, and
`long_history_turns_index_only_the_delta`. The cache measurement line printed:

```
turns=48 rotations=3 new_mean=0.94 old_mean=0.30 new_median=1.00 old_median=0.01 new_high_reuse=94% old_high_reuse=30%
```

This is the deterministic counterpart to the single real rotation above.

## 6. Observations

### 6.1 Turn capacity and completion

The agent held a coherent task across 161–189 model turns and 144–176 tool
calls on a repetitive incremental task. The failure and stagnation supervisors
did not fire in any long run: `validation_failures=0` for all 40 validations in
both runs. Default `max_model_turns` is `None`, so turn count alone is not a
circuit breaker; the observed stop was task completion, not a limit.

### 6.2 Cache efficiency

Cache-read share rose with horizon: 0.774–0.805 on the short Docker tasks,
0.909–0.927 on the long/resume tasks. In every session the first request is
fully uncached; later requests read roughly 91–96% of input. The one aggregate
outlier is the missing cache fields on turn 4 of `lh-taskqueue-01` noted above.

### 6.3 Context rollover costs a re-warm but preserves correctness

The 24k-window run rotated once and still verified. The direct cost is the
re-warm request after rotation (cache read dropped to 4,096 then recovered).
The 32k live run also verified after 189 turns, but because its events were
in-memory the rotation itself is not directly observable there; the persisted
rotation above is the load-bearing evidence.

### 6.4 Self-healing

Two recoveries were observed, both handled without human input:

- `lh-taskqueue-01`: a shell pipeline `find … && echo … && git log` exited 128
  because the fixture was not a git repository; the agent continued and read
  files directly.
- `lh-taskqueue-01`: a `patch` was rejected with
  `stale observation: expected <hash>, found <hash>; re-read before editing`
  after a concurrent read; the agent re-read the file and re-applied the edit.
  This is the hash-guarded write path working as intended.

### 6.5 Isolation

All tool execution stayed inside the Bubblewrap sandbox inside the
capability-stripped container. The only network reachable from the container
was the provider endpoint (`--network bridge`); sandboxed commands had no
network. No run required an outside-workspace write. The repository tree was
clean before and after (`git status --short` empty).

## 7. Threats to validity

- **Single model, single sample.** All runs used `deepseek-v4-flash`; there is no
  cross-model comparison and no repetition, so variance is not characterized.
- **Docker tasks are short.** The one-shot machine CLI and the Python-only
  runtime image limit the Docker fixture to 8–10 model turns. The genuinely
  long runs (161/189 turns) are host live-acceptance runs; they are still real
  provider, real tools, and real Bubblewrap, but not inside Docker. Only the
  24k-window CLI run connects Docker, rollover, and persistence.
- **In-memory live events.** `live_acceptance` does not persist events, so
  per-turn rotation and rollover data are unavailable for the 161/189-turn
  runs. The reported peak there is an estimate, not a sent-request size.
- **`estimated_request_context_tokens` is a high-water estimate.** It is
  `ContextMaterialized.total_tokens` before eviction; the actual request is
  `request_tokens` (12,991 in the 24k run).
- **One missing cache sample.** See §5.1.
- **Offline image build.** The image was built from digest-pinned bases already
  in the local BuildKit cache because Docker Hub was unreachable. Reproducing it
  on a host with registry access should use the repository `docker/Dockerfile`
  directly.

## 8. Reproduce

```sh
# 1. Build a provenance-correct runtime image (online host: use docker/Dockerfile).
docker build --target runtime -f docker/Dockerfile \
  -t latch-dogfood:local --build-arg LATCH_SOURCE_REVISION=$(git rev-parse HEAD) .

# 2. Real-provider Docker task against the fixture.
OPENCODE_API_KEY=… ./scripts/dogfood.sh --no-build --image latch-dogfood:local \
  --config /tmp/latch-dogfood.toml --provider-env OPENCODE_API_KEY \
  --fixture <taskqueue-fixture> --run-id lh-taskqueue-01 --output json \
  --network bridge -- "Read README.md … run 'python3 -m unittest discover -s tests -v' until green."

# 3. Deterministic continuity stress.
cargo test -p latch-kernel --lib continuity -- --nocapture

# 4. Live long-horizon acceptance (real provider, Bubblewrap, tempdir).
LATCH_LIVE_TESTS=1 LATCH_LIVE_SCENARIO=long \
  cargo test -p latch-kernel --test live_acceptance -- --ignored --nocapture
```

The `taskqueue` fixture is not committed (the dogfood harness keeps fixtures
outside the product tree). Its copy from the runs is preserved under
`target/dogfood/lh-taskqueue-01/workspace/` and
`target/dogfood/lh-taskqueue-small/workspace/`; the bug list is: `Task.is_high`
compared against `LOW`; `Task.add_tag` skipped normalization/dedup; `TaskStore.add`
silently overwrote duplicate ids; `TaskStore.pending` included completed tasks;
`parser` failed to lowercase/dedup tags and accepted invalid `!` runs;
`scheduler.next_task` picked the lowest priority and `capacity_report` produced
negative overflow.

## 9. Evidence locations

| Artifact | Path |
| --- | --- |
| Docker run (1M window) | `target/dogfood/lh-taskqueue-01/` (`out/`, `state/latch.sqlite3`, `workspace/`, `provenance.json`) |
| Docker run (24k window, rotated) | `target/dogfood/lh-taskqueue-small/` |
| Docker smoke run | `target/dogfood/smoke-01/` |
| Live reports | `crates/latch-kernel/target/live-acceptance/{long,interrupt}.json` |
| `long` stdout (1M window) | session transcript of this run (`turns=161`, `read=3,517,440`) |
| Offline Dockerfile | `/tmp/opencode/latch-test/Dockerfile.longhorizon` |

Report generated from durable events, CLI JSON output, and live-acceptance
report files only.
