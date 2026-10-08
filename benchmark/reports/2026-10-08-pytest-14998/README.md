# Supervised pytest #14998 CLI reruns

Increasing `context.recent_tokens` from 12,000 to 128,000 eliminated identical
file rereads and context rotations in this observed run. It brought the first
edit forward, but did **not** reduce total elapsed time. Independent acceptance
also found changed call-phase skip retention, so the resulting pytest patch is
not fully accepted for compatibility.

## Task and setup

The task repaired [pytest issue #14998](https://github.com/pytest-dev/pytest/issues/14998)
in the public [pytest repository](https://github.com/pytest-dev/pytest), starting
at commit `3fd8675d6d798507c06cf9c60753be6d9d7b0e17`. Under
`tmp_path_retention_policy=failed`, setup and teardown errors lost temporary
directories. The requested work included a production fix, regression tests,
unchanged `all`/`none` policies, setup-skip cleanup, a changelog entry, and checks.

Both CLI reruns used the installed release binary from Latch commit
`ac4fc319e789bf41d99b1fe66b5b6530c70bcb22`, SHA-256
`d903076da53cf9582d7f84c2b9cfbe4bebbf9e3ae657c5743fedaf406d8d54a4`.
They used the original Chinese prompt without changes, the same dependencies
and permissions, and `opencode-go / deepseek-v4.1-flash / provider_default`.
Each had a fresh clone and separate state directory. Editable installation
paths were redirected to each clone, and its ignored generated version file
was restored. Both baselines confirmed that `failed` retained only
`call_failure`, while `all` and `none` behaved as expected.

The second CLI configuration changed only `context.recent_tokens` to 128,000,
in addition to its isolated state path. Latch product defaults and the global
configuration were not changed. The earlier pytest repair was preserved.
The supervisor inspected events and output without editing the agent's product
patch or inserting guidance into either run.

## Recorded results

| Metric | Earlier TUI, 12k | Updated CLI, 12k | Updated CLI, 128k |
| --- | ---: | ---: | ---: |
| Durable run interval, seconds | 1,390.921 | 522.072, cancelled | 1,527.470 |
| Time to first recorded edit, seconds | 1,130.742 | No edit | 339.416 |
| Tool calls | 254 | 112 | 63 |
| Model requests | 143 | 56 | 48 |
| Context rotations | 48 | 18 | 0 |
| `read_file` calls | 124 | 60 | 15 |
| Identical repeat reads, excluding first occurrences | 70 | 29 | 0 |
| TASK / verifier / scenario reads | 17 / 21 / 19 | 8 / 9 / 8 | 1 / 1 / 1 |
| `validate` calls | 13 | 0 | 4 |
| Managed process start / exit events | 3 / 2 | 2 / 2 | 2 / 2 |

The earlier TUI statistics cover its original development run only, excluding
the later request for a Chinese report. The updated 12k run was cancelled with
SIGINT when the user requested the 128k experiment; it had made no product edits.
Its duration is a cancellation time, not a completed-task performance result.
Read counts include failures, including one denied `/tmp` read in the 128k run.
An identical repeat requires the same arguments and complete tool-result output;
reads of different ranges or versions do not qualify.

Exact data is in [comparison.json](comparison.json). Intervals use durable event
timestamps; the 128k launcher's duration was separately recorded as 1,527.808
seconds, approximately 25 minutes 28 seconds.

## Provider usage and waiting

| Provider-reported metric | Earlier development run | 128k CLI |
| --- | ---: | ---: |
| Input tokens, including cache reads | 1,755,495 | 2,609,049 |
| Cache-read tokens | 1,284,224 | 2,518,144 |
| Cache-miss tokens | 471,271 | 90,905 |
| Output tokens | 142,461 | 51,824 |
| Reasoning tokens | 123,774 | 44,056 |

The larger budget reduced repeated exploration, rotations, and model round
trips, and produced an earlier first edit. Total input increased, while cache
misses and output decreased. No monetary cost was calculated; reasoning tokens
are listed separately, not added to output totals.

Total elapsed time increased. Patch restoration mistakes, additional full-suite
testing, and long model requests occupied the later portion of the run. Model
request start/finish intervals summed to approximately 1,397.6 seconds; the
longest was approximately 207.8 seconds. These intervals can overlap background
tests and cannot be added to test durations or interpreted as reasoning time.

## Issues observed during supervision

1. The updated 12k run still looped through exploration: 18 rotations and 112
   calls without an edit. Excluding kernel snapshots from rotation pressure did
   not eliminate this case's loop at the smaller history budget.
2. A read-only `git stash list` was conservatively classified as Git metadata
   mutation in that run. Machine mode automatically denied its approval request.
3. The 128k agent saved its repair under `/tmp`, temporarily restored baseline
   source to prove the new test failed, and then could not retrieve the backup
   through its workspace policy. Three recovery calls failed. It reapplied the
   three source patches from retained history; the supervisor did not restore
   or repair the source for it.
4. Ruff passed, but an appended `grep` returned 1, making the shell call fail.
5. The first full-suite attempt used an unsupported `--timeout=0` option and did
   not execute the suite. The next produced 4,554 passed, 1 failed, 50 skipped,
   15 xfailed, and 5 xpassed. Both commands piped into `tail`, so process exit 0
   did not certify pytest success. The agent acknowledged the failure output.
6. The agent did not actually read `CONTRIBUTING.rst`, despite announcing it.
7. CLI `final.result.text` concatenated progress text with a kernel completion
   report, without a complete, standalone development summary and diff stat.

## Kernel completion and independent acceptance

The CLI exited 0 with `status=completed` and kernel `completion=verified`.
All three registered requirements passed. A final covering `validate` refreshed
them together after the additional commands. Both managed processes had durable
exit events; the earlier residual-process certification blocker did not recur.
`Verified` covered those registered requirements, not every optional test or
compatibility edge.

Independent checks, recorded in [acceptance.json](acceptance.json), found:

- The three-policy acceptance script passed; auxiliary `.case` files were unchanged.
- The three requested test files produced 223 passed, 1 skipped, and 1 xfailed.
- Ruff checking and format checking passed.
- The new regression failed against original `tmpdir.py`, detecting the baseline bug.
- Both finalizer orderings, setup skip followed by finalizer error, legacy
  `tmpdir` setup/teardown errors, and combined errors retained their evidence.
- **Call-phase skip compatibility failed:** original code retained the directory,
  while the new repair removed it. Separate executions confirmed the difference.
  Accepting this behavior change needs an explicit scope decision; the patch is
  not fully compatibility-accepted.
- The full-suite failure was
  `testing/test_helpconfig.py::test_version_verbose`, with a Hypothesis plugin
  initialization warning. Collecting all of `testing/` and selecting only that
  test passed against both source versions. There was no complete original
  full-suite control, so attribution remains unresolved.

The agent's changes to `src/_pytest/tmpdir.py`, `testing/test_tmpdir.py`, and
`changelog/14998.bugfix.rst` remain in its local checkout for review. No pytest
changes were committed or pushed.

## Evidence and limitations

Local experiment directories retain the original prompt, isolated configuration,
launch metadata, JSONL output, stderr, SQLite events, patches, and audit logs.
This publication contains aggregate metrics and acceptance results; configs,
credentials, SQLite databases, and raw transcripts are not included.

[summarize_events.py](summarize_events.py) reproduces aggregate metrics from a
closed local event database without provider calls:

```sh
python3 summarize_events.py /path/to/latch.sqlite3 SESSION_UUID
```

This is one nondeterministic case study, not a general speed benchmark. The
earlier TUI observation also used an earlier Latch binary. The updated CLI runs
share their setup except for the threshold and isolated paths, but the 12k run
was cancelled. Different test scope and provider latency further prevent a
clean causal comparison of completion time. The evidence supports trying a
larger working-history budget to reduce exploration churn, while assessing
performance and patch correctness separately.
