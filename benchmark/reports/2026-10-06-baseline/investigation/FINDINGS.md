# Benchmark 日志与内核排查

源码 `10c7b12`。读取全部 25 例的 durable SQLite 事件、结果和补丁，对照内核实现；使用当前 release 二进制、本地 SSE mock provider 和 Bubblewrap 做确定性复现。没有追加付费模型调用，没有修改产品代码或原始运行现场。

优先级为本次排查建议，并非正式安全评级。已确认的是下面具体条件下的行为，不代表已证明跨沙箱越权。

## 1. 高：Git 外部 diff 能绕过变更记录，保留失效的 Verified

- 复现：先验证 `x.txt` 内容是 original；隔离仓库的 `diff.external` 配置指向一个仅在工作区写入 x.txt 的 Python helper；执行 `shell: git diff --ext-diff`；调用 complete。
- 结果：x.txt 已变成 `written-by-external-diff`，事件中没有本次 diff 对应的 WorkspaceMutationPossible/FileChanged/ShellMutationObserved，最终 completion=verified，CLI exit=0。
- 根因：`execution/shell.rs:219` 仅根据 git 子命令名认定 diff 等命令只读，没有排除外部 helper。`tools/process.rs:215` 据此跳过预先推进版本和事后漂移检测；WORK 模式的 shell 沙箱仍允许工作区写入。
- 影响：通过已验证文件之后的辅助命令修改工作区，可继续使用旧证据认证完成；变更归属和 undo 记录也可能遗漏。这里未证明网络、外部文件或凭据边界绕过。
- 修复方向：对可执行仓库配置/helper 的 Git 查询保守记录潜在写入，或用只读能力运行明确的查询。不要把命令名白名单当作写入不可能的证明。
- 证据：[repro.json](readonly_git_external_diff/repro.json)。

## 2. 高：验证管道掩盖失败，产生假通过

- 日志：http_ranges 的验证命令是 `python -m unittest -v test_ranges 2>&1 | tail -30`；其他四个用例也使用带 head/tail 的验证管道。本轮不据此断言原始可见测试失败，只确认其退出码可能被掩盖。
- 复现：`validate` 执行 `python3 -B -c "assert False" 2>&1 | tail -3`，随后 complete。
- 结果：输出含 AssertionError，ValidationResult.passed=true，最终 verified，exit=0。
- 根因：`sandbox.rs:381` 用 `/bin/bash -lc` 执行命令，没有开启 pipefail；`tools/process.rs:237` 仅检查复合命令最终退出码。
- 修复方向：为验证执行定义不会默认吞掉管道上游错误的 Bash 语义，保留显式处理预期失败的能力，并增加真实 binary 回归。不能通过扫描输出里的 FAIL/ERROR 判断结果。
- 边界：即使修复 pipefail，validate 仍只能证明所选命令成功，不能自动证明语义 requirement 覆盖用户全部需求。
- 证据：[repro.json](pipeline_false_pass/repro.json)。

## 3. 高：多个可写验证互相失效，形成无法结束的认证循环

- 25 例中全部 13 个 implemented_not_verified 都有多个 required_validations，且旧项的 Passed 证据已过期。它们合计 208 次模型请求、115 次验证通过；这些总数不能全部归因于本缺陷，但日志明确记录了为刷新证据而重复验证的尝试。
- csv_chunks：A 的最新证据 generation=153，B=159，组合项=182；当前 generation=182。组合项通过不能替代原来的 A、B。
- 复现：顺序 validate A、B，命令均为 `python3 -B -c "assert True"`，文件完全未变；随后 complete。两次均 Passed，最终 implemented_not_verified。
- 根因：`tools/process.rs:215-225` 在每个可能写入的命令前推进 generation；`state.rs:270` 只接受当前 generation；`state.rs:84/99` 的 required_validations 只追加、无受控替代语义。每个新验证让之前的证据失效，换名字或在一个模型轮次中提交多次 validate 也不能解决。
- 修复方向：提供同一稳定工作区版本上的验证集合，或能力受限的只读验证执行方式；若提供 requirement 替代，必须保留原要求的覆盖和 durable provenance。不能简单取消写命令的证据失效机制，也不能由模型直接删除未满足要求。
- 证据：[事件审计](event-audit.json)、[repro.json](two_validations/repro.json)。

## 4. 中：只读 .git 过滤被误判为元数据写入

- spool_recovery 在 seq=20 被拒绝的原命令：`ls -la && find . -type f -name "*.py" -o -name "*.md" -o -name "*.toml" -o -name "*.cfg" | grep -v ".git/" | head -50`。
- 根因：`safety.rs:436-450` 的路径扫描仅豁免部分过滤参数，未识别 grep 的模式参数。
- 复现该原命令：permission_denied、exit=3。原 benchmark 的实现最终通过 4/4 独立检查，仍因该拒绝计为失败。
- `cli/machine.rs:358` 保留本次运行的非交互拒绝状态是现有 fail-closed 策略，不应为了提高分数直接清除。应修复误分类并保留真正的 Git 写操作批准要求。
- 证据：[repro.json](readonly_filter_denied/repro.json)。

## 5. 中：省略 complete 可绕过验证纠正流程

- sqlite_migration 没有调用 complete，最终正常退出，status=completed，task.completion=in_progress。
- 复现：shell 修改 x.txt；validate 通过；模型返回纯文本、没有 complete。exit=0，completed/in_progress。
- 根因：`agent.rs:1207-1217` 允许无工具响应结束；`agent/supervision.rs:126-130` 的纠正只处理 ImplementedNotVerified，忽略已经修改工作区但尚未申报完成的 InProgress。
- 影响：没有伪造 Verified，但自动化调用方如果只检查 exit/status，可能把尚未闭环的实现当成交付完成。status 和 task.completion 当前语义不同，应明确区分。
- 修复方向：修改任务的终止路径检查 implementation claim 与证据，保持只读/解释任务的合理退出行为。CLI completed 的契约是否变化需要单独设计。
- 证据：[repro.json](missing_complete/repro.json)。

## 6. 中：validate 不使用配置的命令超时

- `config.rs:849` 默认 shell_timeout_seconds=120；`agent/validation.rs:61-65` 未声明 timeout 时固定使用 600。
- 复现：配置 shell_timeout_seconds=1，同一条休眠 2 秒的命令，shell 在约 1 秒返回 timed out，validate 等到 2 秒并通过。
- cache_stampede 的自写测试在 loader 中设置双人 barrier；single-flight 只会让一个调用进入 loader，另一调用等待该 key，从而阻塞。该 validate 未声明超时；外层 benchmark 在 600 秒时取消它，模型没有机会完成正常恢复。其实现通过了独立 3/3 检查。
- 区分：barrier 是模型测试错误；内核默认超时与配置不一致、外层先取消导致恢复机会不足，是运行流程问题。未证明 timeout/cancel 的沙箱逃逸。
- 修复方向：统一默认超时的来源，为显式长验证保留声明方式；benchmark 的总超时应给单个验证超时后的恢复留余量。
- 证据：[validate 复现](validate_ignores_shell_timeout/repro.json)、[shell 对照](shell_honors_timeout/repro.json)。

## 7. 中：空文本 complete 直接结束，最终总结缺失

- 11 个原始用例的最后一个 assistant_message_completed 包含 complete，但 text 为空。其中 http_ranges 的 CLI result.text 只有开场白 `I'll start by inspecting the workspace files.`。
- 根因：`agent.rs:1238-1280` 的快速退出假定同轮已有总结，不检查非空文本。prompt 要求同轮提供总结，但运行时没有兜底。
- 风险：用户和脚本无法得到修改、验证范围及未验证项的最终解释。某些结果只留下重复验证中的过程文字。
- 修复方向：完成声明无总结时增加一次受限的报告机会，或从内核事实生成最小结果；保留既有的有总结快速退出。
- 证据：[事件审计](event-audit.json)；pipeline_false_pass 复现也触发空文本完成。

## 源码确认的资源风险：输出上限不是内存上限

- `tools/process.rs:269-275` 对 shell stdout/stderr 使用 read_to_end(Vec)，完成后才在 bound_output 中截断/存 artifact。
- managed process 的 `spawn_reader`（:394-406）持续向 String 追加；4 MiB 常量仅在 `finish_process`（:171-178）检查，运行时没有按该阈值滚动落盘，完成后也保留完整 buffer。poll 和 finish 会 clone 全量字符串。
- 持续大输出可能耗尽 Latch 主进程内存，即使子进程处于沙箱中。这是可用性风险，不是已证明的提权。只运行了有限 5 MiB 输出 fixture，未执行 OOM 或无限输出攻击，未测定实际崩溃阈值。
- 修复方向：流式 artifact 写入、内存中的有界 tail、从 artifact 按 cursor 分页，超时/取消后统一 drain/abort reader 任务。
- 超时分支未采用 managed terminate 的 kill_sandbox_children 且不主动 abort reader，另有待进一步复现的清理风险；当前不把它列为已证实泄漏。

## 三个实现失败与 benchmark 规格问题

1. http_ranges：代码确实返回 416；失败是第三个返回值为 None，而独立检查要求 `bytes */3`。模型新增可见测试也使用 None 作为预期。因此“未处理不可满足范围”的初步描述不够准确。问题是返回格式覆盖不足、自写测试强化了错误预期。用例 prompt 未直接写出 416 的 content_range 格式，建议补充规范。
2. append_index：模型主动 truncate 不完整尾部后追加；独立检查要求抛 ValueError 并保持原文件字节不变。它不是简单漏写检查，而是采用了不同恢复语义，存在数据丢失风险。prompt 的 “do not append over a partial tail” 可以被理解为先修复，建议明确禁止自动 truncate。这个 fixture 的文件不是 Latch 产品自身的 durable event log，不能据此宣称 Latch 删除了历史。
3. lease_queue：修改只修复 token 检查，未拒绝 ttl=0。独立检查包含该要求，但 prompt/原始可见测试没有明确说明 ttl 必须为正。建议补充规则后重复实测；当前仍保留原始失败分数。

独立检查缺乏明确规格的问题会影响分数解释，但不影响上述真实二进制确定性复现出的内核问题。不要通过降低独立验收标准掩盖缺陷，也不要把所有 fixture 错误归因于内核。

## 建议修复顺序

1. Git helper 写入漏记、验证管道退出状态：先防止错误认证。
2. 同一版本的验证集合：消除普遍的重复验证与无法认证。
3. 非交互误分类、缺失 complete、空总结、默认超时：修复正常交付闭环。
4. 流式输出资源边界和 benchmark 规格澄清。

## 重现

`reproduce.py` 和 `reproduce-timeout.py` 使用本机 loopback mock、虚拟凭据、短命令和新建隔离工作区，不调用付费模型。需要真实 Bubblewrap 命名空间权限。

```sh
python3 benchmark/reports/2026-10-06-baseline/investigation/reproduce.py
python3 benchmark/reports/2026-10-06-baseline/investigation/reproduce-timeout.py
```

默认报告写入新的 ignored benchmark/runs/investigation-* 目录；可用 LATCH_INVESTIGATION_OUT 指向一个新目录。原始 benchmark 和本次 repro 的数据库、结果与事件均保留。
