# Latch Linux benchmark candidate suite

This directory contains **25 self-contained candidate cases** across three
difficulty tiers: 7 easy, 7 medium, and 11 hard. They model real operational
workflows but are independently constructed, **not verified reproductions of
specific upstream issues**. They use Python's standard library so Latch can
focus on debugging rather than downloading dependencies. Difficulty labels
describe the intended reasoning and state surface; they have not yet been
calibrated by repeated model runs.

| Tier | Case | User situation | Independent acceptance |
| --- | --- | --- | --- |
| Easy | `stream_records` | A log collector receives UTF-8 JSON lines in arbitrary network byte chunks. | Every byte boundary, multiple records, final line and empty reads. |
| Easy | `csv_chunks` | A CSV importer receives quoted rows across network reads. | Embedded commas/newlines and all split positions. |
| Easy | `path_rules` | A file picker applies ordered ignore and reinclude rules. | Last matching rule wins. |
| Easy | `ansi_width` | A terminal table aligns colored Unicode cells. | ANSI controls, combining marks and wide glyphs. |
| Easy | `option_values` | A deployment CLI handles explicit empty values and `--`. | Value presence, positional parsing and errors. |
| Easy | `header_merge` | An HTTP gateway aggregates repeated headers. | Case-insensitive names and separate cookies. |
| Easy | `duration_units` | A scheduler parses millisecond and larger time units. | Units, zero and malformed inputs. |
| Medium | `config_layers` | A CLI combines defaults, a project file, environment and flags. | Nested precedence, empty and false values, caller input ownership. |
| Medium | `pagination_cursor` | An activity feed paginates while earlier records may be deleted. | Stable keyset cursor and invalid input. |
| Medium | `http_ranges` | A download endpoint serves byte ranges. | Inclusive, suffix, open-ended and invalid ranges. |
| Medium | `retry_policy` | An HTTP client retries transient responses. | Method safety, Retry-After and retry budget. |
| Medium | `archive_paths` | A document importer extracts user ZIP files. | Traversal/symlink preflight and normal extraction. |
| Medium | `sqlite_migration` | An address book upgrades its SQLite schema. | Preserved rows, idempotence and malformed schemas. |
| Medium | `log_rotation` | An operations daemon rotates bounded log archives. | Archive order, retention and invalid limits. |
| Hard | `spool_recovery` | A job runner resumes an append-only spool after failure or interruption. | Retry, incomplete line, UTF-8 byte offset and stale checkpoint. |
| Hard | `lease_queue` | Workers reclaim expired jobs. | Stale acknowledgements, attempts and concurrent claims. |
| Hard | `webhook_dedupe` | A receiver stores durable event claims. | Handler failure rollback and duplicate delivery. |
| Hard | `dag_scheduler` | A build scheduler executes dependent tasks. | Ordering, shared dependencies, cycles and failures. |
| Hard | `atomic_config` | A daemon writes JSON settings and a backup. | Invalid writes, recovery and corrupt copies. |
| Hard | `incremental_sync` | A backup tool mirrors a source directory. | Content hashes, deletion, unchanged files and links. |
| Hard | `stream_framing` | An RPC server decodes length-prefixed messages. | Arbitrary chunks, multiple frames and size limits. |
| Hard | `cache_stampede` | Concurrent callers share slow cache loads. | Single flight, retry after failure and key independence. |
| Hard | `append_index` | An event log rebuilds byte offsets after partial writes. | UTF-8 offsets, hidden tails and append refusal. |
| Hard | `rate_window` | A gateway limits requests per tenant. | Rolling boundaries, isolation and denied requests. |
| Hard | `transaction_outbox` | An order service persists rows and outgoing events. | Atomic rollback, duplicates and event order. |

Each `workspace/` contains the buggy project and visible tests. Acceptance
checks stay outside the copied workspace in `acceptance.json` or `check.py`.
The shared checker runs before and after the agent in a networkless Bubblewrap
sandbox. That sandbox exposes only the selected case and workspace read-only,
hides the host home directory, and gives checks private scratch space. The
runner requires at least one failing baseline check and never modifies the
case template. Every case has a `reference/` repair for author verification;
the runner never copies it into Latch's workspace. An isolated Git repository
and Latch state directory are created for every attempt.

Validate the complete case set without a model call:

```sh
python3 benchmark/verify_cases.py
```

This checks that each baseline fails at least one independent check and that
the reference repair passes all independent checks and visible tests.

## Run

Linux requires the usual Latch prerequisites (`bwrap`, `rg`, Git) and a working
DeepSeek v4.1 Flash profile. Set the credential environment variable named in
your Latch configuration. The runner copies that configuration and redirects
state to a private run directory; it does not copy secret files. If your
configuration has an explicit top-level `state_dir`, provide a version without
that setting. Check prerequisites with `latch doctor` first.

```sh
cargo build --release --locked
python3 benchmark/run.py list
python3 benchmark/run.py run --case stream_records \
  --config ~/.latch/config.toml
```

For a legacy installation, pass its `~/.config/latch/config.toml` instead.
Repeat `--case` to select several cases, or use `--case all`. The runner pins
the CLI invocation to provider `opencode-go`, model `deepseek-v4.1-flash`, and
`WORK` mode. The source config must define that provider and its credential.
Each case has a wall-clock timeout; a timeout terminates the CLI process group.

Reports go to ignored `benchmark/runs/<timestamp>-<pid>/`. Each case contains
`result.json`, CLI stdout/stderr, a Git patch including new files, an isolated state directory,
and the final workspace. `summary.json` aggregates case results. Correctness
requires every independent check to pass, a successful CLI exit, and the
requested provider/model. `completion` is recorded separately, since a passed
external acceptance suite and Latch's own `Verified` state answer different
questions. Event counts are read from the root session's durable SQLite log;
child-agent activity remains reflected in the CLI graph usage totals.

Token usage and cache reads come from the provider's reported graph usage.
Cost is `null` unless `--pricing` supplies a dated JSON price snapshot with
`input_per_million`, `cached_input_per_million`, and `output_per_million` rates
in USD. Example:

```json
{
  "input_per_million": 0.0,
  "cached_input_per_million": 0.0,
  "output_per_million": 0.0
}
```

Replace those placeholders with rates valid for the run. Reports record the
exact rates used. Timing currently measures end-to-end CLI wall time, including
model and tool waits; it does not attribute individual phases. Only the
original `stream_records` and `config_layers` cases have been smoke tested
with a real model. The full 25-case set needs repeated runs and difficulty
calibration before it can support a comparative coding-agent score.

## OpenCode comparison

The optional local comparison runner uses the same prompts, pristine case
workspaces and independent evaluator. It requires an installed `opencode`,
Bubblewrap and `OPENCODE_GO_API_KEY` (use the same credential as Latch):

```sh
python3 benchmark/run_opencode.py --case spool_recovery \
  --case stream_records --case cache_stampede --jobs 3
```

It starts private OpenCode servers with the `build` agent and the same
`opencode-go/deepseek-v4.1-flash` model. The entire OpenCode process runs in
Bubblewrap with the host home hidden, isolated state and a writable attempt
directory; independent acceptance checks and reference repairs are not exposed
to the agent. Provider network access remains enabled. Each attempt retains the
case's existing timeout. Reports live in ignored `benchmark/runs/opencode-*`.
Credentials and databases stay local and must not be included in exported
reports. Concurrency is capped at three.

The runner normalizes OpenCode's cached input into total input. OpenCode v2
separates text/tool output and reasoning; their sum is the comparable output
count. Latch already reports total input and total output. Costs remain null
without a dated price snapshot. Compare external acceptance rather than
equating OpenCode termination with Latch's kernel `Verified` state. Single
attempts on three cases are exploratory, not an overall agent ranking.

For a full 25-case comparison, use one scheduler for both agents:

```sh
python3 benchmark/compare.py --latch-config ~/.config/latch/config.toml --jobs 3
python3 benchmark/report_comparison.py benchmark/runs/comparison-<stamp>-<pid> \
  benchmark/reports/<report-name>
```

The scheduler randomizes paired case order with a recorded seed and limits
total concurrency across both agents to three. It also requires GNU
`/usr/bin/time` and records CPU time and maximum single-process RSS (including
waited-for descendants, not aggregate process-tree memory).
OpenCode's private server is incompletely accounted by this launcher-level
measurement; raw CPU/RSS logs are retained but cannot compare agent resource
usage. A complete process-tree sampler is needed for that comparison. Source
and binary hashes, case hashes, scheduling order and progress are retained locally. A
failed attempt is recorded without silently retrying it. Model tokens for
interrupted Latch runs can be recovered from durable events, but unfinished
provider requests may remain unreported.

The exporter requires a dated `deepseek-pricing.json` snapshot in the run
directory with official sources and separate cache-hit, cache-miss and output
rates. It reports peak/off-peak USD/CNY estimates, averages including failed
attempts, cost per successful case including failed-attempt spending, tier
statistics and per-attempt CSV data. These estimates apply direct DeepSeek API
prices to observed OpenCode Go usage and are not actual Go invoices. Keep
original configs and SQLite databases private; the exporter checks credential
values before publishing compressed event records and logs.
