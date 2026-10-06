# Full 25-case Latch / OpenCode comparison

Source `170efcbb3b5f0507f9e757c71c5340c9049cc523`; OpenCode `opencode v2.0.22`. Same Linux host, model `deepseek-v4.1-flash`, credential, prompts, fixtures, timeouts and external checks. One attempt per agent per case; at most three attempts globally. Randomized paired order, seed 42; exact schedule and hashes in summary.json.

## Overall performance

| Metric | Latch | OpenCode |
| --- | ---: | ---: |
| Passed cases | 23/25 | 22/25 |
| External checks | 81/83 | 80/83 |
| Timeouts | 0 | 0 |
| Total input tokens (including cache) | 1656820 | 2135835 |
| Total output tokens (including reasoning) | 94632 | 133497 |
| Total tokens | 1751452 | 2269332 |
| Cache-hit fraction | 85.0% | 91.6% |
| Mean seconds / attempt | 34.776 | 49.923 |
| Median seconds / attempt | 31.304 | 49.546 |
| P90 seconds / attempt (nearest rank) | 55.605 | 79.901 |
| Sum of task wall seconds | 869.409 | 1248.086 |
| Model turns (graph) | 206 | 247 |
| Tool calls (graph) | 260 | 291 |

On the 22 identical cases both agents passed, mean seconds were 33.410 for Latch and 50.171 for OpenCode. Paired successful-case distributions are also retained in metrics.json.

| Tier | Latch passed | OpenCode passed | Latch mean seconds | OpenCode mean seconds |
| --- | ---: | ---: | ---: | ---: |
| easy | 7/7 | 7/7 | 26.539 | 40.110 |
| medium | 7/7 | 7/7 | 34.359 | 52.573 |
| hard | 9/11 | 8/11 | 40.284 | 54.483 |

## Cost estimate

Prices retrieved 2026-10-06 from [DeepSeek CNY pricing](https://api-docs.deepseek.com/zh-cn/quick_start/pricing/) and [USD pricing](https://api-docs.deepseek.com/quick_start/pricing/). Per million tokens: off-peak CNY miss 1 / hit 0.02 / output 4; peak 2 / 0.04 / 8. USD off-peak 0.15 / 0.003 / 0.60; peak 0.30 / 0.006 / 1.20. These are separate official currency price lists, not FX conversions.

Estimated cost = uncached input × miss rate + cached input × hit rate + total output × output rate, divided by 1,000,000. Cached tokens are already included in input; reasoning is already included in normalized output. These are hypothetical direct DeepSeek API prices applied to observed OpenCode Go usage, not actual Go invoices. Both time-of-day scenarios are shown; no billing period is assigned to Go calls.

| Agent / rate | Mean uncached input cost | Mean cache-hit cost | Mean output cost | Mean total / attempt | Total / 25 attempts | All-attempt cost / passed case |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| latch / off_peak CNY | 0.009932 | 0.001127 | 0.015141 | 0.026200 | 0.655006 | 0.028479 |
| latch / peak CNY | 0.019865 | 0.002254 | 0.030282 | 0.052400 | 1.310012 | 0.056957 |
| opencode / off_peak CNY | 0.007195 | 0.001565 | 0.021360 | 0.030119 | 0.752974 | 0.034226 |
| opencode / peak CNY | 0.014389 | 0.003130 | 0.042719 | 0.060238 | 1.505949 | 0.068452 |

## Per-case results

| Case | Latch checks | OpenCode checks | Latch passed | OpenCode passed | Latch seconds | OpenCode seconds | Latch tokens | OpenCode tokens |
| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: |
| ansi_width | 3/3 | 3/3 | True | True | 30.869 | 59.502 | 72513 | 109214 |
| append_index | 2/3 | 2/3 | False | False | 33.562 | 55.678 | 68508 | 94455 |
| archive_paths | 3/3 | 3/3 | True | True | 34.721 | 49.345 | 68168 | 106371 |
| atomic_config | 3/3 | 3/3 | True | True | 71.93 | 93.691 | 133619 | 157153 |
| cache_stampede | 3/3 | 3/3 | True | True | 32.604 | 35.436 | 56953 | 57288 |
| config_layers | 4/4 | 4/4 | True | True | 46.25 | 33.57 | 121099 | 50828 |
| csv_chunks | 3/3 | 3/3 | True | True | 41.84 | 50.289 | 90277 | 60824 |
| dag_scheduler | 4/4 | 4/4 | True | True | 24.992 | 60.096 | 43830 | 129881 |
| duration_units | 3/3 | 3/3 | True | True | 14.5 | 24.27 | 30211 | 43990 |
| header_merge | 3/3 | 3/3 | True | True | 21.593 | 50.705 | 38919 | 85113 |
| http_ranges | 3/3 | 3/3 | True | True | 37.331 | 31.153 | 82837 | 57036 |
| incremental_sync | 4/4 | 4/4 | True | True | 46.456 | 59.784 | 77339 | 126140 |
| lease_queue | 3/4 | 3/4 | False | False | 31.304 | 49.546 | 65569 | 91402 |
| log_rotation | 3/3 | 3/3 | True | True | 27.693 | 74.832 | 71915 | 140624 |
| option_values | 3/3 | 3/3 | True | True | 22.952 | 33.469 | 39387 | 62689 |
| pagination_cursor | 3/3 | 3/3 | True | True | 23.085 | 82.168 | 48397 | 136136 |
| path_rules | 3/3 | 3/3 | True | True | 11.656 | 20.65 | 27336 | 46118 |
| rate_window | 4/4 | 4/4 | True | True | 19.618 | 30.957 | 40622 | 55119 |
| retry_policy | 4/4 | 4/4 | True | True | 24.269 | 44.103 | 50690 | 74623 |
| spool_recovery | 4/4 | 4/4 | True | True | 31.224 | 79.901 | 69748 | 169946 |
| sqlite_migration | 3/3 | 3/3 | True | True | 47.164 | 52.838 | 72105 | 89330 |
| stream_framing | 3/3 | 3/3 | True | True | 26.314 | 36.042 | 62259 | 86172 |
| stream_records | 4/4 | 4/4 | True | True | 42.362 | 41.882 | 99898 | 86035 |
| transaction_outbox | 3/3 | 2/3 | True | False | 69.515 | 39.094 | 104328 | 50771 |
| webhook_dedupe | 3/3 | 3/3 | True | True | 55.605 | 59.085 | 114925 | 102074 |

## Failed acceptance checks

- latch / append_index: append_rejects_partial_tail; CLI exit 0, timeout False, completion verified.
- latch / lease_queue: invalid_lease; CLI exit 0, timeout False, completion verified.
- opencode / append_index: append_rejects_partial_tail; CLI exit 0, timeout False, completion not applicable.
- opencode / lease_queue: invalid_lease; CLI exit 0, timeout False, completion not applicable.
- opencode / transaction_outbox: duplicate_has_no_extra_event; CLI exit 0, timeout False, completion not applicable.

Both agents failed zero-TTL rejection in lease_queue and refusal to append over a partial tail in append_index. OpenCode also implemented duplicate orders as INSERT OR IGNORE (a successful no-op) in transaction_outbox; its independent check expects duplicate rejection. These include underspecified prompt contracts: positive TTL and duplicate rejection are not explicit, and partial-tail wording admits different recovery interpretations. Scores and checks are preserved unchanged. Latch's Verified certifies its declared validation commands, not the undisclosed acceptance suite; its failed cases can still be Verified.

## Interpretation and evidence

One run per case is exploratory; random sampling, provider load and cache state can change results. Correctness uses independent acceptance plus successful CLI exit and exact model, not a comparison of Latch Verified with OpenCode termination. Failed cases remain included in overall averages; successful-only distributions and tier metrics are in metrics.json. All-attempt cost per pass includes spending on failed attempts, not the mean cost of passing attempts alone.

Wall time is monotonic CLI elapsed time including cold startup, provider and tool waits; task sums are not parallel batch wall time. Raw GNU time CPU/RSS are retained, but OpenCode's sandbox/private server is incompletely accounted (launcher RSS around 2 MiB is not the agent's RSS). These values are not comparable agent CPU/memory measurements and are omitted from the comparison table. A future memory comparison needs sampling/cgroup accounting of the complete process tree. Missing resource samples are not imputed as zero. No OS cache flush or provider phase attribution was performed.

Token usage is provider-reported durable session activity. OpenCode v2 cache reads/writes are added to input and reasoning is added to output to match Latch's totals. If a timeout leaves a provider request unfinished, durable usage is a lower bound and the invoice may be higher. Pricing estimates hold observed cache hits constant; direct DeepSeek API could have different cache behavior. No claims of statistical significance or third-party benchmark validity are made.

Original isolated workspaces and databases remain in ignored `benchmark/runs/comparison-20261006T060117Z-77350`. Exported evidence excludes configs, credentials and databases. Per-attempt patches, stdout/stderr, resource logs and compressed durable records are under cases/. CSV includes input/output/cache usage and per-attempt estimated CNY cost; metrics.json includes USD/CNY scenarios and tier distributions.
