# Latch Linux benchmark pilot

This directory contains three **self-contained pilot cases**, not verified
reproductions of upstream issues. They exercise the benchmark runner and a
difficulty ladder before a larger real-project suite is selected. The cases
use Python's standard library so Latch can focus on debugging rather than
downloading dependencies.

| Tier | Case | User situation | Independent acceptance |
| --- | --- | --- | --- |
| Easy | `stream_records` | A log collector receives UTF-8 JSON lines in arbitrary network byte chunks. | Every byte boundary, multiple records, final line and empty reads. |
| Medium | `config_layers` | A CLI combines defaults, a project file, environment and flags. | Nested precedence, empty and false values, caller input ownership. |
| Hard | `spool_recovery` | A job runner resumes an append-only spool after failure or interruption. | Retry, incomplete line, UTF-8 byte offset and stale checkpoint. |

Each `workspace/` contains the buggy project and visible tests. The sibling
`check.py` stays outside Latch's copied workspace and runs after the agent
exits. The runner checks that at least one acceptance check fails before each
run; it never modifies the case template. An isolated Git repository and
Latch state directory are created for every attempt.

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
`result.json`, CLI stdout/stderr, a Git patch, an isolated state directory,
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
model and tool waits; it does not attribute individual phases. This pilot is
for validating the harness and understanding case quality. It is too small
and synthetic to claim a general coding-agent score.
