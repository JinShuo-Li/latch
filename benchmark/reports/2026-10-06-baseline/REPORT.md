# Latch 全量 benchmark 本机实测

- 源码：`10c7b124767d74e3809db197e5b753ea945af145`；release 二进制 SHA256：`465ef37972aaa543874ac274a75a07b9bd23cb7db4c44ebc83cfe25954fd9ec1`。
- 模型：`opencode-go / deepseek-v4.1-flash`；每例一次，最多同时三例。
- 25 个用例的故障基线和参考修复均验证通过。
- 完整成功：20/25（80%）；独立验收全部通过：22/25（88%）；检查项：80/83。
- 实际总耗时：18.10 分钟；各例耗时合计：2308.995 秒。
- Token：输入 2,878,761，输出 202,325，缓存读取 2,378,496（82.62%）。未提供定价快照，不估算费用。
- 298 次模型请求，382 次工具调用，134 次验证通过，5 次验证失败。
- 完成状态：verified 10，implemented_not_verified 13，in_progress 2。

| 难度 | 完整通过 |
| --- | --- |
| easy | 7/7 |
| medium | 6/7 |
| hard | 7/11 |

## 五个失败用例

| 用例 | 原因 |
| --- | --- |
| http_ranges | 无效/不可满足范围检查失败；Latch 自身仍为 verified。 |
| append_index | 没有拒绝在不完整尾部后追加。 |
| lease_queue | 没有拒绝零 TTL。 |
| spool_recovery | 独立验收 4/4；出现需要 Git 元数据写入批准的请求，非交互模式拒绝，CLI exit 3。 |
| cache_stampede | 独立验收 3/3；模型自行编写的验证脚本阻塞，600 秒用例超时。脚本在同一 key 的 loader 中设置双人 barrier，可能与 single-flight 行为冲突。 |

## 后续排查重点

1. 完成状态与通过的验证证据不一致，以及重复验证；此处仅记录现象，根因尚未确认。
2. 验收覆盖不足：已 verified 的 http_ranges 仍漏掉独立检查。
3. 模型验证脚本的阻塞与超时恢复。
4. 非交互权限拒绝如何影响最终退出状态。

## 每例结果

| 用例 | 完整通过 | 独立检查 | 完成状态 | 秒 |
| --- | --- | --- | --- | ---: |
| [ansi_width](cases/ansi_width/result.json) | 是 | 3/3 | implemented_not_verified | 167.714 |
| [csv_chunks](cases/csv_chunks/result.json) | 是 | 3/3 | implemented_not_verified | 95.926 |
| [duration_units](cases/duration_units/result.json) | 是 | 3/3 | implemented_not_verified | 154.983 |
| [header_merge](cases/header_merge/result.json) | 是 | 3/3 | implemented_not_verified | 43.353 |
| [option_values](cases/option_values/result.json) | 是 | 3/3 | implemented_not_verified | 58.913 |
| [path_rules](cases/path_rules/result.json) | 是 | 3/3 | verified | 11.424 |
| [stream_records](cases/stream_records/result.json) | 是 | 4/4 | verified | 31.682 |
| [archive_paths](cases/archive_paths/result.json) | 是 | 3/3 | verified | 24.804 |
| [config_layers](cases/config_layers/result.json) | 是 | 4/4 | implemented_not_verified | 50.652 |
| [http_ranges](cases/http_ranges/result.json) | 否 | 2/3 | verified | 25.612 |
| [log_rotation](cases/log_rotation/result.json) | 是 | 3/3 | implemented_not_verified | 131.407 |
| [pagination_cursor](cases/pagination_cursor/result.json) | 是 | 3/3 | verified | 28.066 |
| [retry_policy](cases/retry_policy/result.json) | 是 | 4/4 | implemented_not_verified | 79.877 |
| [sqlite_migration](cases/sqlite_migration/result.json) | 是 | 3/3 | in_progress | 51.458 |
| [append_index](cases/append_index/result.json) | 否 | 2/3 | implemented_not_verified | 167.07 |
| [atomic_config](cases/atomic_config/result.json) | 是 | 3/3 | implemented_not_verified | 181.059 |
| [cache_stampede](cases/cache_stampede/result.json) | 否 | 3/3 | in_progress | 600.149 |
| [dag_scheduler](cases/dag_scheduler/result.json) | 是 | 4/4 | verified | 19.598 |
| [incremental_sync](cases/incremental_sync/result.json) | 是 | 4/4 | verified | 77.452 |
| [lease_queue](cases/lease_queue/result.json) | 否 | 3/4 | implemented_not_verified | 40.508 |
| [rate_window](cases/rate_window/result.json) | 是 | 4/4 | implemented_not_verified | 55.335 |
| [spool_recovery](cases/spool_recovery/result.json) | 否 | 4/4 | implemented_not_verified | 124.31 |
| [stream_framing](cases/stream_framing/result.json) | 是 | 3/3 | verified | 20.663 |
| [transaction_outbox](cases/transaction_outbox/result.json) | 是 | 3/3 | verified | 26.48 |
| [webhook_dedupe](cases/webhook_dedupe/result.json) | 是 | 3/3 | verified | 40.5 |

本轮不重试、不修改产品代码或用例。本目录保留报告、补丁和日志；隔离工作区和 SQLite 原件留在 ignored benchmark/runs/ 下。该结果是单次运行基线，不代表重复测量后的稳定成功率。
