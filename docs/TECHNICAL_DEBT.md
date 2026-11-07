# Technical debt assessment

Baseline: `c91acf1` on `main`, inspected 2026-11-07. The working tree was clean
and fetched `origin/main` matched HEAD. This is a new assessment;
[the earlier 25 findings](../CODE_REVIEW_FINDINGS.md) are resolved historical
evidence, including CR-11's corrected false positive.

## Fixed issues

Severity describes impact, not demonstrated exploitability. Reproductions and
validation below distinguish code evidence from native-host qualification.

Sources: [request assembly](../crates/latch-kernel/src/agent/request.rs),
[wire adapters](../crates/latch-kernel/src/provider.rs),
[process execution](../crates/latch-kernel/src/tools/process.rs), and
[group dogfood](../crates/latch-kernel/tests/group_dogfood.rs).

| ID | Severity / module | User impact and evidence | Repair and regression evidence |
| --- | --- | --- | --- |
| TD-01 | Medium / `agent/request.rs::context_messages` | Adjacent assistant turns merge content but discard the later turn's reasoning artifacts and reasoning content. Earlier signed text can also suppress merged plain text in Gemini serialization. Code reproduction: two completed assistant events without intervening user/tool events. | Keep reasoning-bearing turns separate. `adjacent_assistant_reasoning_survives_context_reconstruction` failed before (one turn instead of three), passes after. |
| TD-02 | Medium / `provider.rs::GeminiStreamState` | A complete nonempty JSON argument prefix followed by malformed fragments leaves earlier arguments executable; non-object JSON and invalid wire types can also become tool arguments/default `{}`. | Validate final fragments as objects and reject malformed types; preserve object/fragment/id-less and omitted-argument calls. `gemini_stream_rejects_malformed_final_argument_fragments` failed before, passes after; signature-update regression also passes. |
| TD-03 | Medium / `provider.rs::checked_response_redacted` | `response.text()` buffers an arbitrarily large error body before the 4 KB diagnostic limit applies. A stalled error body can hide the known HTTP status until a header deadline. | Bound ingestion to 16 KiB, preserve typed status/Retry-After, redact partial credentials at the boundary. `http_error_body_is_bounded_before_eof_and_keeps_retry_facts` failed before (2-second timeout), passes after; ASCII/UTF-8 credential-boundary and interrupted-body regressions pass. |
| TD-04 | Medium / `tools/process.rs::run_process_inner` | Ordinary shell timeout/cancellation kills only the runner, unlike managed termination's platform descendant cleanup. Reader tasks are detached on early return. | Reuse `kill_sandbox_children` and wrap readers in `AbortOnDropHandle`. Real sandbox `shell_cancellation_and_timeout_stop_descendant_writes` passes on Linux before and after; it is a regression guard, not proof of a prior Linux survivor. The new native macOS smoke check also passed; no macOS pre-fix reproduction is claimed. |
| TD-06 | Medium / `tests/group_dogfood.rs` | Quoted regex validation lies outside the conservative read-only grammar, so concurrent fixture workers invalidate each other's passes and can exhaust 80 model turns. Observed in the full gate. | Use an explicit-file read-only check, retain real command execution and assert exact final file contents. `fixture_validation_never_invalidates_peer_evidence` failed before and passes after; no classifier, evidence freshness, retry budget or completion gate was relaxed. |
| TD-07 | Low / `provider.rs::USER_AGENT` | All requests identify v0.3.4 as `latch/0.2.3`, contrary to the constant's documented version contract. | Derive the header from Cargo at compile time. `user_agent_tracks_the_package_version` failed before, passes after. OpenCode endpoint/session headers and model selection are unchanged. |

Fix commits: `50607bd` (TD-01), `0b767e4` (TD-02), `df2e460` (TD-03),
`3509463` (TD-04), `3786da5` (TD-06), `6f3d5a3` (TD-07).

## Remaining confirmed debt

| ID | Severity / source | Impact, evidence and required next work |
| --- | --- | --- |
| TD-05 | Medium / `tools/process.rs::run_process_inner` | Ordinary shell output uses `read_to_end` before artifact bounding. A sufficiently chatty command can exhaust host memory within its timeout. Managed output already spills during execution. A repair must stream complete stdout/stderr to artifacts, retain existing output ordering and UTF-8 behavior, and test disk-full/cancellation paths. No full-output data may be silently discarded. |

TD-04's fix scopes explicit timeout/cancellation and reader ownership. It does
not bound successful commands' output drain after runner exit, or prove cleanup
at every native startup phase. Those paths need native process/pipe fixtures;
compilation and ordinary successful commands do not establish qualification.

## Qualification gaps and intentional limits

- [Windows issue #7](https://github.com/JinShuo-Li/latch/issues/7), including its
  qualification comment, remains open for physical power loss, hostile host
  rename/reparse/hardlink races, unrelated ACL edits, whole live-home scale and
  remaining application/architecture qualification. These are not confirmed
  vulnerabilities. See [Windows diagnostics](WINDOWS_DIAGNOSTICS.md) and
  [native recovery](../native/windows/boundary/RECOVERY.md).
- macOS 14 runtime, Intel/Rosetta, hostile filesystem races, deliberate process
  group escape, Developer ID signing and notarization remain outside current
  qualification. See [macOS](MACOS.md). Native CI exercises macOS 15 ARM64.

## Architecture and compatibility decisions

The six requested large modules were inspected by responsibility and callers,
not judged by line count. `provider.rs` retains wire-specific serialization;
`providers.rs` owns catalogs/configuration and registry construction;
`continuity.rs` intentionally keeps rotation/episodes/recall together;
`store.rs` owns transactional durable state/projections; `interactive.rs` is the
shared TUI/Web controller; protocol types own durable and extension contracts.
Their test sections contribute substantially to size. No mechanical split is
justified by size alone. TD-01/02 target independently testable transformations.

Legacy provider materialization, continuity re-exports, replay-only events and
optional protocol fields still have supported callers or durable compatibility
roles. No obsolescence was established; none should be removed in this cleanup.
Store group claims/delivery use immediate transactions with concurrency tests;
validation generations and completion publication already have dedicated
regressions. Preserve those ownership boundaries and evidence semantics.

## CI evidence

- [Baseline CI](https://github.com/JinShuo-Li/latch/actions/runs/42248118489)
  passed Linux, Windows and macOS smoke checks.
- [Prior full validation](https://github.com/JinShuo-Li/latch/actions/runs/41609684865)
  passed at `c2ae4c4`; this predates the baseline and is not qualification of
  every subsequent change.
- [Pages failure](https://github.com/JinShuo-Li/latch/actions/runs/42248118942):
  the build succeeded, deployment was created, remained queued then updating,
  and hit the deploy action's 600,000 ms timeout. No source/build defect is
  established; increasing limits or changing site content lacks supporting
  evidence.
- Default smoke includes real Bubblewrap/AppContainer/Seatbelt execution and
  Windows/macOS Web transport. Manual full validation retains full Linux,
  serial Windows Rust and native recovery/broker fixtures. Paid/live/stress
  suites remain outside default CI.

## Validation

- The three parsing/ingestion regressions were run against the old code and
  failed, then passed after their repairs. New socket fixtures use actual
  loopback HTTP; shell tests use mandatory Bubblewrap, without a fallback.
- Focused continuity/cache/long-session tests: 15 passed. Existing Gemini tests,
  including valid fragmented and parallel calls: 19 passed before the final
  added HTTP fixture; the complete suite is the final authority. Group dogfood passed five
  consecutive runs after its fixture correction.
- Offline installer checks: 15 passed; Web Markdown checks: 5 passed.
- Baseline release gate reached a transient `ETXTBSY` starting a freshly copied
  disposable uninstall binary. The unchanged focused uninstall suite passed on
  rerun. This is an observed test/environment reliability concern without an
  established source cause; no retry or test suppression was added.
- Final `bash scripts/release-gate.sh`: passed, including the real Bubblewrap
  namespace/mount probe, formatting, all-target/all-feature locked Clippy with
  warnings denied, architectural invariants (22 passed), workspace tests
  (951 passed, 10 opt-in tests ignored), and locked release build.
- Site build self-check, build and HTML validation: passed (30 HTML pages,
  1,829 local links/assets/fragments, 241 search sections). Offline reference
  recovery: 6 passed; benchmark runner checks: 4 passed; Web version check: 1 passed.
- [Pages on the repair commit](https://github.com/JinShuo-Li/latch/actions/runs/42339192143)
  passed build and deployment; deployment took 17 seconds with no Pages workflow
  change. This supports the operational-timeout diagnosis for the baseline run.
- [Repair smoke CI](https://github.com/JinShuo-Li/latch/actions/runs/42339192256)
  passed on Linux, Windows and macOS at `3786da5`, including native Seatbelt
  cancellation/timeout, CLI/Web startup and real TUI PTY checks.
- [Full validation](https://github.com/JinShuo-Li/latch/actions/runs/42339192564)
  passed at `3786da5`: Linux release gate, strict native Windows build,
  recovery/filesystem/process/broker fixtures, sandboxed Cargo, serial Windows
  Rust/runtime/integration tests and Windows release build.
- The later User-Agent-only correction `6f3d5a3` passed the final local release
  gate above and all 9 OpenCode-focused tests. No native boundary code changed
  after the full validation run. Final branch smoke runs are listed in the
  [CI workflow](https://github.com/JinShuo-Li/latch/actions/workflows/ci.yml).
  Native execution was remote, not local. No live/paid provider tests run.
