# 七项修复与验证

每项产品修复均为独立 Conventional Commit；基线结果、原分析、测试 fixture 更新和本次验证证据分别提交。原始基线保持不变。

| 问题 | 修复 | 验证 |
| --- | --- | --- |
| Git 外部 helper 漏记写入 | 判定只读的检查命令实际使用只读文件系统能力；显式写能力仍推进工作区版本 | 外部 diff helper 写入失败，原文件不变，旧验证仍真实有效 |
| 验证管道吞掉上游失败 | Linux validate 开启 pipefail；Windows 拒绝不能认证上游成功的管道 | assert False 管道不再 Passed；Windows 解析测试通过 |
| 多个验证互相失效 | validate 可用 requirements 在一次命令、同一版本证明多个已有要求 | A/B 共同刷新、resume 一致；后续写入仍使全部旧证据失效 |
| grep 的 .git 过滤误拒绝 | 仅识别 grep -v 的模式参数，保留真正 Git 元数据写入审批 | 原 spool 命令成功，危险写入分类回归通过 |
| 缺失 complete 绕过纠正 | 已记录修改且 InProgress 时也给予一次受限纠正回合 | 真实 CLI 在模型补 complete 后 Verified；忽略纠正仍保持 InProgress |
| validate 默认超时忽略配置 | 使用 shell_timeout_seconds，保留显式覆盖 | 配置 1 秒阻止 2 秒命令通过；显式 3 秒允许通过 |
| 空 complete 缺少总结 | 由内核输出修改文件及当前验证状态 | 无额外请求、无伪造 assistant 事件；过期证据显示 unverified |

`bash scripts/release-gate.sh` 最终 exit 0：格式、Clippy（全部 target/features）、架构 invariants、全部工作区测试及 release 构建通过。`continuity` 的 14 个长会话/缓存测试通过。真实 release CLI、本地 SSE mock 和 Bubblewrap 的七个独立 Git 工作区回归全部通过；运行结果及原始事件存于 runtime/。

完整测试发现若干旧 mock 在新纠正回合耗尽响应；仅更新相应 fixture，并保留原有断言。CLI JSONL 测试确认忽略纠正时仍保持 InProgress，正常退出不等于 Verified。监督测试必须以具备 Bubblewrap 命名空间权限的进程运行。

复现（不调用付费模型）：

```sh
cargo build --release --locked
python3 benchmark/reports/2026-10-06-fixes/verify_runtime.py
```

默认输出新建 ignored benchmark/runs/investigation-*；具体本次来源和二进制哈希见 provenance.json。付费三例复测单独记录于 ../2026-10-06-regression/。这里的本机验证为 Linux；Windows 实际运行结果由推送后的 CI 单独确认。

原分析额外列出的输出缓冲资源风险及 benchmark 规格歧义不属于这七项，尚未在本次修复；不能将本报告解释为所有潜在问题已经消除。
