# Working-history threshold calibration — 2026-10-09

The maintainer selected **64,000 conversation tokens** as the default after
six paid DeepSeek v4.1 Flash attempts through OpenCode Go on the same preserved
pytest #14998 task. Testing has stopped. This is a practical choice for this
case, not a universal optimum.

| Threshold | Attempt | Rotations | Identical reads/searches | First edit (s) | Wall time (s) | Stop |
| --- | --- | --- | --- | --- | --- | --- |
| 32,000 | 1 | 5 | 4 / 0 | 256.096 | 532.338 | Completed |
| 64,000 | 1 | 1 | 0 / 0 | 128.238 | 485.176 | Completed |
| 64,000 | 2 | 2 | 0 / 0 | 173.404 | 339.675 | Completed |
| 96,000 | 1 | 1 | 0 / 0 | 80.741 | 1531.635 | Input limit |
| 96,000 | 2 | 0 | 0 / 0 | 98.309 | 391.196 | Input limit |
| 128,000 | 1 | 0 | 0 / 0 | 194.556 | 467.670 | Completed |

Both 64k attempts crossed rotation boundaries without identical reads/searches.
The 32k attempt repeated four reads before its first edit, immediately after
its first rotation. The first 96k attempt repeated two shell results after
editing; revalidation is not automatically wasted work. Both 96k attempts hit
the input-token ceiling, so their durations are censored.

**Patch correctness remains a separate limitation:** every attempt passed the
main policy verifier and focused upstream tests but failed the independent
call-phase skip compatibility check. None passed full external acceptance;
these measurements do not establish successful repair quality. Case materials
were unchanged. Small samples and a single task limit generalization.

The [aggregate results](results.json) record provider usage and independent
checks. Raw credentials, configs, transcripts and databases remain private in
ignored run directories. The [runner documentation](../../README.md#working-history-threshold-calibration-linux-opt-in-paid)
describes baseline isolation, ceilings and repeat detection. No live tests are
part of CI. The 64k default retains roughly 48k in whole semantic units after
rotation, subject to the hard request budget, while preserving durable history.
