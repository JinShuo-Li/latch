# 2026-10-06 baseline artifacts

One attempt per case on Linux, at most three concurrent attempts, using the
release binary for source revision 10c7b12 and opencode-go/deepseek-v4.1-flash.
See REPORT.md and summary.json for measurements, source and binary provenance.

Each cases/<id>/ contains independent acceptance results, the final patch, CLI
JSON output, stderr, and gzip-compressed JSONL durable events. Decompress with
`gzip -dc cases/<id>/events.jsonl.gz`. Events retain SQLite rowids so validation
generations and call lineage can be reconstructed without publishing databases.
Absolute paths in original case results/events describe the measured host; they
are provenance, not dependencies. Source configuration, credentials, .git trees,
binaries, and regenerable state/artifact directories are excluded.

The original local run is benchmark/runs/full-20261006T044158Z/.
This is an uncalibrated single-run baseline, not a comparative agent score.
