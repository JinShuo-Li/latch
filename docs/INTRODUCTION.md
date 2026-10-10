# Introduction

Latch is an open-source coding agent written in Rust for Linux and Windows.
It provides a terminal interface, a local Web interface, and a machine CLI.
You choose a model provider and supply its credential.

## Start here

[Install Latch](INSTALL.md), open a terminal in your project, and run `latch`.
Use `/setup` to configure a provider and model, then run `latch doctor` to
check local prerequisites without contacting the provider.

```sh
latch
# Or open the local browser interface:
latch --web
```

See [terminal and CLI usage](../README.md#get-started) and the
[Web interface guide](WEB_UI.md).

## Execution and evidence

Commands run inside a mandatory platform sandbox: Bubblewrap on Linux or the
embedded native AppContainer runner on Windows. If the sandbox is unavailable,
command execution fails. Modes, safety policy, and approval handling control
which operations may proceed. Sandboxing does not establish that a change is
correct; recorded validation provides evidence for the workspace state it checked.
Later writes make earlier passes stale.

Sessions persist as durable events in SQLite. Working-history rotation bounds
model requests while keeping the original history available. Resume restores
session state and evidence. Root agents can delegate to independent child
sessions; child reports do not certify the root task.

Read about [working safely](../README.md#working-safely),
[continuity](CONTINUITY.md), and [multi-agent coordination](ARCHITECTURE.md#durable-child-agent-graph).

## Integration status

Agent Skills and MCP tool servers are **implemented in v0.3.2**. Skills are
instructions loaded on demand, not permission grants. MCP supports tool discovery
and execution over sandboxed stdio or Streamable HTTP. Remote HTTP services run
outside Latch's local sandbox. Windows stdio servers reconnect for each call;
stateful Windows servers need Streamable HTTP.

MCP resources, prompts, sampling, elicitation, subscriptions, OAuth flows, and
asynchronous tasks are not exposed. Computer use and several replaceable runtime
ports remain deferred. See [Skills and MCP](SKILLS_AND_MCP.md) for supported
behavior and [runtime capabilities](RUNTIME_CAPABILITY_MODEL.md#8-implemented-now-vs-deferred)
for the implemented and deferred boundary.

## Supported platforms and limits

Precompiled binaries target x86_64 Linux (glibc 2.35+) and Windows. Linux needs
Bubblewrap, ripgrep, and Git. Windows needs Git, ripgrep, and a focused user-owned
NTFS workspace. The native Windows sandbox is embedded in the binary.

Windows qualification does not cover every host or workspace: an entire live
home directory and hostile host races remain unqualified. Read the
[Windows findings](WINDOWS_DIAGNOSTICS.md) and
[boundary status](../native/windows/boundary/CURRENT_STATE.md) before relying on
behavior beyond the tested scope.

## Source and recorded results

Latch is MIT licensed. [Source and issues](https://github.com/JinShuo-Li/latch)
are on GitHub. The [25-task comparison](../benchmark/reports/2026-10-06-full-comparison/README.md)
records one attempt per task with a specific model and host; it is an exploratory
measurement, not a general performance guarantee.
