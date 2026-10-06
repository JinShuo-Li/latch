# OpenCode 与修复后 Latch：三例本机对照

同一台 Linux 主机、同一凭据、同一 OpenCode Go endpoint 与 DeepSeek v4.1 Flash；三个 case 的 prompt、模板和独立验收完全相同，case_sha256 逐项一致。每个代理每例一个正式 attempt，最多三个同时运行。

Latch 取 ../2026-10-06-regression/ 的修复后实测；OpenCode 本机版本 v2.0.22，build agent、private standalone server、自动批准隔离目录操作。OpenCode 进程由外层 Bubblewrap 限制写入，隐藏本机 home、验收和 reference；保留 provider 网络。两种代理自身工具、提示词及验证机制不同。

| Task | 检查（两者） | Latch 秒 | OpenCode 秒 | OpenCode 耗时差 | Latch 总 token | OpenCode 总 token | OpenCode token 差 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| spool_recovery | 4/4 | 48.346 | 69.841 | +44.5% | 115,973 | 74,452 | -35.8% |
| stream_records | 4/4 | 36.822 | 56.167 | +52.5% | 61,047 | 90,847 | +48.8% |
| cache_stampede | 3/3 | 24.983 | 89.381 | +257.8% | 62,526 | 89,649 | +43.4% |

两者均 3/3 通过、独立检查合计 11/11。Latch 三例总 token 239,546；OpenCode 254,948，多 6.4%。三例任务耗时之和分别 110.151 秒与 215.389 秒，OpenCode 多 95.5%；这是各任务耗时之和，不是并发批次 wall time。Latch 耗时少 48.9%。

| Token 口径 | Latch | OpenCode |
| --- | ---: | ---: |
| 总输入（含缓存） | 228,959 | 239,582 |
| 总输出（含推理） | 10,587 | 15,366 |
| 缓存命中（已含在输入） | 188,288 | 207,488 |

缓存命中率：Latch 82.2%，OpenCode 86.6%。未命中输入：Latch 40,671，OpenCode 32,094。OpenCode 总量略高但未命中输入更少；不同 token 费率下费用差未必与总量差相同，未提供定价，费用保持 null。

## 解释与局限

- 这一轮 Latch 三例都更快；token 方面，spool_recovery 是 OpenCode 更省，另两例是 Latch 更省。不能推断全部 25 例或其他模型的总体排名。
- Latch spool_recovery 有 11 次模型回合、16 次工具调用及 5 次验证通过；OpenCode 有 8 次模型回合、10 次工具调用。这里值得继续检查 Latch 的重复验证与探索开销；不能直接把所有差异归因于单一机制。
- OpenCode v2 把未缓存输入、cache read/write 分开；把三者相加才与 Latch input_tokens 对齐。OpenCode v2 的 output 不含 reasoning，所以将 output + reasoning 与 Latch completion_tokens 对齐。已逐项核对 session_message 累计值与 session_v2 聚合值一致，三个会话 idle_outcome 都是 succeeded。
- 两者的外部验收同样通过；Latch 自身的 Verified 与 OpenCode succeeded 不是同一种认证，不能拿它们直接比较验证强度。
- 两组是不同时间的单次运行，含模型等待、工具执行和 CLI/私有服务器冷启动。provider 负载、缓存状态、采样随机性及服务器开销均可能影响时间和 token；没有归因这些因素，也未用重复运行估计置信区间。
- 早期隔离启动调试（临时路径冲突、v2 空 provider catalog）没有产生模型调用，不作为正式 attempt。正式结果的初始汇总读取了旧数据库 schema，已从原始 v2 durable records 重新提取模型和 token 后修正判定；没有重跑、挑选更优输出或改变验收。

## 证据与复现

原现场 `benchmark/runs/opencode-20261006T054623Z-74156/` 保留隔离工作区与数据库。本目录仅导出结果、补丁、JSONL 输出和压缩消息记录，不含凭据/配置/数据库。源码及二进制哈希见 opencode-summary.json；Latch 来源见 ../2026-10-06-regression/summary.json。

```sh
python3 benchmark/run_opencode.py --case spool_recovery --case stream_records --case cache_stampede --jobs 3
```

运行方式按 [OpenCode CLI 文档](https://opencode.ai/docs/cli/)；显式 provider 配置按 [OpenCode provider 文档](https://opencode.ai/docs/providers/)。本机 v2 模型目录初始为空，因此显式定义同 endpoint/model 的 OpenAI-compatible provider，开启 reasoning_content 交错回放；配置不修改本机 OpenCode 或 Latch 安装。
