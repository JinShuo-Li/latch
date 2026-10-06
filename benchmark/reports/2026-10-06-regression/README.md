# 修复后的付费回归

源码 `4ad60f04f0b236ff1b8f970f52e9791b032a356d`，release SHA-256 `aaaf98791fc68e5108d7115d14f02a77df1daa6426d698793595d51f084fbdba`。

Linux 本机，三个用例各运行一次，最多三个并发；同原始基线的 opencode-go/deepseek-v4.1-flash 配置。没有修改用例或验收标准。

| 用例 | 独立检查 | completion | 秒 |
| --- | --- | --- | --- |
| spool_recovery | 4/4 | verified | 48.346 |
| stream_records | 4/4 | verified | 36.822 |
| cache_stampede | 3/3 | verified | 24.983 |

三个用例均通过且 Verified。原基线 spool_recovery 是 permission_denied，cache_stampede 是 600 秒超时；本次未重现这些结果。模型输出具有随机性，三例单次复测不能推出完整 25 例的新通过率，也不能把全部时间差归因于内核修复。

原始 ignored 现场：`benchmark/runs/regression-20261006T053148Z/`。本目录保留结果、补丁、CLI 输出和压缩 durable 事件；不发布配置、凭据或 SQLite 数据库。定价未配置，费用保持 null。
