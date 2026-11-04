# LSP usability qualification — 4 November 2026

## Scope and provenance

This is a small, paid **usability experiment**, not an agent ranking or a
statistical success-rate benchmark. It tests whether a real model can choose and
use Latch's native LSP tool, and whether the resulting user workflow completes.

- Host: Linux under WSL2; mandatory Bubblewrap boundary enabled.
- Provider: the operator's existing configured OpenCode Go account.
- Model: `deepseek-v4.1-flash`, provider-default reasoning effort.
- Language server: Ubuntu clangd 18.1.3, already installed; background indexing,
  clang-tidy and repository clangd configuration disabled.
- Qualified source: `aa7c6225ab4086a43fe18a52a0e48832bf9ac6fb`.
- Binary SHA256: `9dd05e74e431f3fce68467149497c0a7564cdb4d16dac6e6605517bf22bf6459`.
- Each attempt: fresh fixture workspace, Git baseline, config and state; at most
  12 model turns, retry budget one, and a 240-second outer wall limit.
- No existing user configuration, credentials or conversation history modified.
  Credentials were resolved in memory, not copied into reports/configuration.

See [runner and reproduction instructions](../../README.md)
and [design/qualification limits](../../../docs/LSP.md).

## Final qualification run

The prompts describe navigation and repair tasks, not scripted tool arguments.
Acceptance checks examine actual durable tool results, workspace contents,
validation evidence and the presence of a substantive navigation answer.

| Scenario | Actual LSP actions | LSP errors | Time | Outcome |
| --- | --- | ---: | ---: | --- |
| Distinguish integer and floating overloads | status, symbols, references ×2, hover, definition | 0 | 26.450s | Passed: correct overload-specific locations and a concrete user answer; no writes |
| Diagnose and repair integer pricing | diagnostics | 0 | 19.895s | Passed: guarded repair, recorded validation and independent hidden behavior checks |
| Missing server → text-tool fallback | status, symbols | 1 expected startup failure | 21.629s | **Not fully passed**: text-tool fallback worked, but the final response omitted the requested explanation |

**Result: both configured-server LSP tasks passed; strict end-to-end usability
acceptance passed 2/3 scenarios. Do not describe this as an unconditional 3/3.**
The missing-server run did not crash or fabricate semantic results, but a normal
exit plus a generic completion report is not a satisfactory navigation answer.

### What was independently checked

The navigation fixture has two `billing::quote` overloads. Integer references
must include the declaration at `main.cpp:2` and call at `main.cpp:6`, without
including the floating declaration/call at lines 3/7. The final answer accurately
reported signature `int billing::quote(int n)`, return type `int`, both reference
sets and unknown index completeness. The source remained byte-for-byte unchanged.

The repair fixture starts with an invalid string argument and an incorrect
integer rate. The model used diagnostics, inspected source, patched it and
recorded a passing build/run validation. A separate, offline, credential-free
Bubblewrap sandbox compiled a hidden harness checking integer inputs 0, 3 and 7,
floating inputs 2.0 and 4.0, and the candidate program's own result. All passed;
changing only the visible expected answer could not satisfy the hidden harness.

For fallback, the configured executable deliberately does not exist. The model
received the actionable startup failure and then used ordinary file inspection
instead of installing software or obtaining wider permissions. However, it ended
with `complete` and no substantive final answer. Existing empty-completion policy
returned an honest generic kernel report, not an answer to the question. This
remains a model/workflow limitation, not a passing scenario. This feature does
not change the general completion policy or add hidden paid summary requests.

### Reported usage for the final three attempts

| Scenario | Input tokens | Cached input | Output tokens |
| --- | ---: | ---: | ---: |
| Overload navigation | 30,953 | 23,424 | 1,596 |
| Diagnose and repair | 42,871 | 35,456 | 623 |
| Missing-server fallback | 25,001 | 12,160 | 1,288 |

These are provider-reported counts, not billed cost estimates.

## Development observations, including failures

Ten paid attempts were made across development and final qualification; earlier
attempts are not silently substituted for the final run:

1. Initial configured-server navigation/repair both worked. The first fallback
   attempt was interrupted by a provider connection reset after semantic output;
   automatic replay was correctly suppressed. Only that scenario was explicitly
   rerun, and text-tool fallback worked after two startup failures.
2. Reviewing a navigation answer revealed an unsupported claim about background
   indexing. Results/context were tightened to say `index_completeness: unknown`.
3. A subsequent invocation-only check looked successful, but manual review found
   empty final navigation answers. Acceptance was strengthened rather than
   treating successful tool calls or CLI exit as task success. The session now
   explicitly requests concrete navigation answers before completion.
4. The final run above fixed the configured-server navigation answer but still
   exposed the fallback-answer limitation. It is retained as a failure, not
   rerun until a lucky answer passes.

Across all ten attempts, recorded process wall time was 235.645s, with reported
337,097 input tokens, 224,640 cached input tokens and 10,414 output tokens. An
interrupted remote generation can have unreported usage; these totals are not a
claim of a complete provider invoice.

## Limits and next work

- Tiny C++ fixtures and one final sample per scenario do not establish large-repo,
  long-session or all-model quality.
- Cross-project index completeness is deliberately unknown. The conservative
  clangd configuration does not qualify full-workspace references.
- Real-server/model qualification here is Linux/WSL + clangd, not native Windows,
  macOS or Rust/Python language-server qualification.
- Navigation-answer presence is a minimal deterministic heuristic, not a semantic
  grader; final answers and tool traces were also reviewed manually.
- A targeted future improvement is read-only completion/answer handling after a
  tool fallback, while preserving explicit request budgets and durable semantics.

Raw configs, databases and transcripts remain private under ignored
`benchmark/runs/lsp/`. The accompanying `summary.json` contains reviewed aggregate
metadata only; no credentials, user paths or conversations are published.
