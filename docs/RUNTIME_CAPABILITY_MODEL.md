# Runtime capability model

> Kernel = authority. Extensions and backends = replaceable mechanisms and
> policy.

Latch is a Linux-first terminal coding agent today. This document defines the
runtime model that lets it grow into a capability-oriented agent runtime
platform — Computer Use, remote execution, remote extensions, service exposure,
app-protocol clients such as Feishu/Slack/Telegram adapters, LayerFS-like
workspaces, MCP, custom context engines, and alternative multi-agent
coordinators — **without redesigning the kernel and without loosening the
invariants that make Latch trustworthy**.

It is deliberately conservative about what exists now. The model is implemented
where current coupling made the abstraction useful immediately
([`ContextEngine`](#the-contextengine-port-implemented), the capability
vocabulary), and documented as a port map where it is intentionally deferred.

## 1. Kernel invariants that no extension, backend, or client may bypass

These are properties of the kernel, not policies extensions opt into. Every
mechanism added under this model must preserve all of them.

1. **Session identity.** A session id is durable, created once, and never
   forgeable. Child agents are independent durable sessions with their own
   ids. Provider transports that use session metadata (OpenCode Go's stable
   `x-opencode-session`) derive it from the durable id, never from an external
   caller. A remote client or extension cannot impersonate a session.
2. **Durable event ordering.** The raw event log is the source of truth. Events
   receive a monotonic per-session sequence from the single kernel append path
   and are never deleted or lossily summarized. Resume replays the same order.
3. **Transaction semantics.** Multi-event transitions that must survive resume
   commit atomically (for example group task claims use `BEGIN IMMEDIATE`
   compare-and-set inside the transaction that appends the event). No mechanism
   may append partial kernel truth or claim success before its durable commit.
4. **Cancellation.** Every run has exactly one cancellation token. Provider
   calls, tool calls, managed processes, permission waits, extension RPCs, and
   future remote backends obey it. A slow or hostile extension can never pin a
   cancelled run.
5. **Permission enforcement.** Mode (work eligibility), Safety
   (`Allow`/`Ask`/`Deny` classification), Permissions (resolver), and the
   mandatory Bubblewrap sandbox compose into one pipeline. Approval is a
   durable request/resolution pair plus a single-use `CapabilityGrant` keyed by
   kernel call id — never model-supplied, never extension-supplied. Hard deny
   (privileged/system-destructive) is independent of profile and resolver.
6. **Evidence provenance.** `passed`/`failed` evidence is kernel-owned, created
   only by the kernel executing a validation and recording its real source
   event. The model, extensions, clients, and backends may report observations
   (`pending`, `unavailable`) but can never self-certify. Child-agent evidence
   never becomes root evidence; only a semantic report crosses the boundary.
7. **Tool-call/result integrity.** Every tool call gets exactly one terminal
   result in the same turn; a call/result transaction is never split by
   rotation or delivery. Provider-visible history is sanitized so no provider
   ever observes a dangling call or result.
8. **Secret isolation.** Credentials resolve at process start from
   `env:`/`file:`/`keyring:` references. Secret values never enter
   configuration, the durable event log, model context, transcripts, or
   ordinary logs; provider error bodies are redacted. New transports and
   backends inherit this rule, including authenticated remote clients.
9. **Crash/restart semantics.** Durable transitions fail closed: live state is
   never reported successful before its event commits. A persisted `Starting`
   or `Running` child with no surviving runtime reconciles once to
   `Interrupted` and is never rerun implicitly; managed processes report
   honestly that they did not survive the restart; resume reconstructs state,
   evidence, failure, progress, change ownership, and cache epochs from durable
   events without re-executing anything.

## 2. Capability vocabulary (implemented)

`crates/latch-kernel/src/capability.rs` names what a session can offer and
attaches explicit facts to every declaration:

| Fact | Meaning |
|---|---|
| **kind** | `workspace`, `executor`, `context`, `tools`, `computer`, `browser`, `service`, `artifacts`, `agents` |
| **owner** | `Kernel`, `Session(id)`, `Extension(name)`, `Client(name)`, `Service(name)` |
| **lifetime** | `Session(id)`, `Run(id)`, `Connection`, `Call(id)` |
| **scope** | `Session`, `Workspace { root }`, `Network { hosts, ports }`, `Remote { endpoint }` |
| **permission ceiling** | the sandbox vocabulary (`workspace_read`, `network_access`, …); declaring a permission never grants it |

`CapabilityId` is a validated symbolic name (`workspace.primary`). A
`CapabilityDescriptor` is a kernel declaration. A `CapabilityHandle` is an
immutable issued value with **no API that widens scope, extends lifetime, or
upgrades permissions**. The `CapabilityRegistry` is a small per-session
declaration list: `resolve` answers a `CapabilityRequest` only from
declarations the kernel made, and scope containment is strict and
one-directional (a request under `workspace.primary/`'s root resolves; a
request for `/etc` or for an undeclared `computer` surface does not).

This is not a service container. There is no universal service lookup, no
mutable plugin context, and no registration path for model- or
extension-supplied code. `Agent::capabilities()` exposes the declarations for
audit and for future transport negotiation; it grants nothing.

Current declarations from a root session (child sessions declare the same
classes without `agents.supervisor`):

| Id | Kind | Owner | Scope |
|---|---|---|---|
| `workspace.primary` | workspace | session | workspace root |
| `executor.sandboxed` | executor | session | workspace root |
| `context.engine` | context | kernel | session |
| `tools.kernel` | tools | kernel | session |
| `artifacts.session` | artifacts | session | session |
| `agents.supervisor` | agents | kernel | session |

Unimplemented surfaces are simply absent. `computer`, `browser`, and `service`
appear only when their mechanism exists and is configured.

## 3. Replaceable runtime ports

A **port** is a narrow internal contract between the kernel and one replaceable
mechanism. Port rules:

- **Request/result objects, not kernel internals.** A port never hands out the
  `EventStore`, a SQLite handle, the agent loop, or a mutable kernel context.
- **Kernel authority is not transferable.** Implementing a port in-process
  means running with the authority that port has. Ports are therefore
  operator-installed, trusted components. Extensions and remote clients never
  implement kernel ports; they reach these surfaces only through
  kernel-mediated, permission-checked calls.
- **One contract, one concern.** Prefer small, well-named contracts. Do not
  create a trait per struct or a generic container that erases ownership.
- **Durable results stay provider-neutral.** Ports exchange durable protocol
  events and plain request/result types; wire formats live only in provider
  adapters.

### The `ContextEngine` port (implemented)

`crates/latch-kernel/src/context.rs` defines:

```text
ContextEngine::materialize(ContextRequest) -> ContextView
ContextEngine::{config, set_config, set_estimator, default_budget,
                manual_compact, recall, name}
```

- `ContextRequest` carries the session id, canonical `TaskState`, retrieval
  query, evidence ledger, failure manager, the session-independent compiled
  system prompt and the session-specific context, `ContextBudget`, extension
  context, and re-ground instruction. It borrows live kernel state but holds no
  storage handle.
- `ContextView` carries the session-independent system prompt, the
  session-specific context, the provider-visible `recent` events of the current
  durable cache epoch, rendered canonical / recalled text, episodes, bridge,
  and `ContextStats`. The agent sends `system` as the provider system field and
  renders `session_context` (workspace, repository instructions, mode) as the
  first provider-visible message, keeping the `system` + tools prefix
  session-independent and cacheable.
- The default implementation is `ContinuityEngine`; every behavior of the
  existing engine is unchanged — L0–L3 memory, canonical authority, SQLite FTS
  recall, episodes, cache epochs, hysteretic rotation, `/compact`, resume
  equivalence, and cache accounting all remain exactly as documented in
  [`CONTINUITY.md`](CONTINUITY.md).
- The historical names `MaterializedContext` and `MaterializeBudget` remain
  valid aliases (`ContextView` / `ContextBudget`) so durable semantics and
  existing callers are untouched.
- Replacement is **runtime-wide**. `AgentRuntime` selects the root engine;
  `Agent::set_context_engine_factory` installs the one `ContextEngineFactory`
  policy that `AgentSupervisor` uses to build the engine for every child
  session — new spawns, workers rebuilt after a restart, and children
  reconstructed on process resume. The factory receives only a
  `ContextEngineSpec` (child session id, effective inference profile, and
  context configuration), never an agent, mutable kernel state, or a storage
  handle. `ContinuityEngine` roots default to
  `continuity_context_engine_factory`, which is exactly
  `ContinuityEngine::for_model(...)`, so ordinary construction stays simple.
  Any other root engine must install a matching factory; until it does, child
  spawn fails closed instead of silently running a different engine than the
  root. `AgentSupervisor` never constructs `ContinuityEngine` directly.

Port-specific kernel invariants, enforced by tests in
`crates/latch-kernel/tests/invariants.rs`:

1. the runtime consults the configured engine for every request and never
   falls back to the default implementation behind the caller's back;
2. the provider sees exactly the view the engine returned (the port cannot
   inject a parallel path);
3. the default engine produces byte-identical views, epoch accounting, and
   recall when called directly or through `dyn ContextEngine`;
4. the port exposes no store handle, so a replacement cannot mutate durable
   session truth outside the structured view it returns;
5. a configured child policy governs every child: the factory-built engine
   serves child requests, no continuity kernel context appears in the child
   session, and a resumed child reconstructs through the same factory from its
   durable/effective profile;
6. the default factory is behaviorally equivalent to
   `ContinuityEngine::for_model(...)`, and a non-default root engine without a
   child policy fails child spawn loudly rather than falling back.

A radically different engine (for example a remote service that materializes
context from an external index) is possible because the contract has no
`EventStore` dependency; such an engine must still return protocol-valid,
bounded views and is installed by the operator, not by the model.

### Port map: implemented now vs intentionally deferred

| Port | Status | Today's mechanism | Planned contract shape |
|---|---|---|---|
| `ProviderAdapter` | **implemented as `ModelProvider`** | `provider.rs` (`OpenAiProvider`, `OpenAiResponsesProvider`, `AnthropicProvider`), `ProviderRegistry`, `ProviderFactory` | Keep the trait; add adapters, not a second abstraction |
| `ToolProvider` | **implemented as `ToolExecutor` + extension registration** | `tools.rs` dispatch; `extension.rs` registered tools; kernel tools in `agent/request.rs` | A future `ToolProvider` port sources tool definitions + execution into the same dispatch and policy pipeline |
| `ContextEngine` | **implemented** (`context.rs`) | `ContinuityEngine`; `ContextEngineFactory` propagates the policy to all child sessions (default: continuity) | New implementations replace the default at root and child level; contract stable |
| `Coordinator` | **partially implemented, concrete** | `AgentSupervisor` (execution/lifecycle), `GroupCoordinator` (durable claims/mailbox) | A `Coordinator` port would let an alternative coordination strategy plug in while root truth, child sessions, and group durability stay kernel-owned |
| `PreferenceProvider` | **partially implemented, concrete** | `Config`, `PolicyEngine`, `PermissionBroker` (approval resolution) | A port for user/operator preferences (approvals, model preference, safety defaults) with the same durable resolution rules |
| `WorkspaceBackend` | **deferred** | local filesystem inside `ToolExecutor`; sandbox mounts | Planned: path operations (`read`/`write`/`list`/`hash`/`watch`) with change-ledger and guarded-edit semantics owned by the kernel |
| `ExecutorBackend` | **deferred** | `SandboxRunner` + `tools/process.rs` | Planned: spawn/poll/terminate contract; sandbox profile construction stays kernel-owned |
| `ComputerBackend` | **deferred** | none | Planned: screen/input actions over a local or remote endpoint, as permission-classified tool calls |
| `BrowserBackend` | **deferred** | none | Planned: navigation/DOM/screenshot actions, same policy pipeline |
| `ServiceProvider` | **deferred** | none | Planned: host-managed exposure of a local service through an authenticated, cancellable channel |

No traits are introduced for deferred ports until a real implementation needs
them; the capability vocabulary already names their kinds and scope shapes so
the first implementation does not require redesign.

## 4. Runtime extensions vs backends vs clients vs MCP

Four categories, four trust levels. Confusing them is the main way a platform
loses its invariants.

| Category | What it is | Trust | How it reaches the kernel |
|---|---|---|---|
| **Runtime extension** | an out-of-process program started under the mandatory sandbox, speaking framed JSON-RPC over stdio: tools, commands, observe/transform/guard hooks, context sources | cooperative, below kernel authority | only through `ExtensionRegistry`; tools pass safety classification; observations are non-authoritative context sources; the model never sees raw hook payloads |
| **Backend** | a replaceable mechanism for a kernel-owned port (context, workspace, executor, computer, browser, service) | trusted, operator-installed, in-process with the kernel | through its port contract; if it appends durable events it does so as structured kernel context, not arbitrary truth |
| **Client** | a steering/observing surface (the TUI today; a remote Feishu/Slack/Telegram adapter later) | authenticated but untrusted with kernel internals | through an app protocol: submit user turns, observe semantic events, resolve pending permissions through kernel-mediated requests |
| **MCP** | an external capability integration standard: MCP servers publish tools/resources/prompts | external, own protocol | as a capability source behind the extension/remote-host boundary; MCP is **not** Latch's internal plugin ABI |

Only the kernel appends authoritative events. Extensions, clients, and MCP
servers can influence what the model *sees* (context sources, tool results,
user turns) and can be denied, but they can never forge evidence, session
identity, or kernel truth.

## 5. Transport independence

The extension protocol is transport-agnostic by construction:

- the wire format is LSP-style `Content-Length` framing over JSON-RPC 2.0
  (`extension.rs`, `docs/PROTOCOL.md`);
- `FramedReader<R>`/`FramedWriter<W>` are generic over `AsyncRead`/`AsyncWrite`,
  and the framing round-trip is tested over an in-memory duplex stream, not a
  child process;
- the current spawn path (sandboxed child process with piped stdio) is one
  transport binding, not part of the protocol.

A future remote extension host may therefore carry the same frames over a Unix
socket, TCP, or WebSocket, or tunnel them through a remote RPC session, without
redesigning the kernel. What is deliberately not implemented yet: remote
transport bindings, authentication for them, and connection lifecycle
management. The rule that must hold when they land is unchanged — transport
never confers authority; the kernel still classifies, approves, and cancels.

Remote **clients** are a different surface. They speak an authenticated app
protocol (submit user intent, receive semantic events, resolve permissions) and
never reuse the extension tool ABI as a control channel.

## 6. No universal hook, no raw state access

- **No universal mutable hook.** Extensions register specific, named
  capabilities (`tool.register`, `hook.observe`, `hook.transform`,
  `hook.guard`, `context_source.register`). There is no catch-all interceptor
  with a mutable kernel context, no ability to rewrite arbitrary kernel state,
  and no middleware chain around the event log.
- **No raw kernel state or SQLite.** Extensions and remote clients never
  receive an `EventStore`, SQLite handle, artifact database, or permission
  broker. They observe only the semantic events the kernel chooses to forward,
  and they may append nothing to the durable log.
- **Hooks are advisory, never authoritative.** A guard can narrow (deny/ask)
  but cannot widen a decision past the safety profile or the sandbox; an
  observe hook receives a copy; a transform hook returns a new value that the
  kernel validates before use (for example an invalid `model_request`
  transform is rejected, not applied).

## 7. Acceptance examples

These examples are design checks, not implemented features. Each one must be
possible under this model without changing kernel invariants.

### LayerFS-like workspace

A `WorkspaceBackend` implementation serves reads/writes/list/hash over virtual
paths. Latch declares `workspace.primary` with `Workspace { root }`, owner
`Session`, lifetime `Session`, and the sandbox vocabulary permission ceiling.
The kernel still owns guarded-edit hashes, the change ledger, artifact spills,
and sandbox mount construction; the backend is a mechanism. A remote or layered
filesystem must reject writes outside its declared root — the kernel checks the
request against the declaration, and the backend cannot widen it. The backend
never appends `FileChanged`; only kernel-controlled mutation paths do, so
provenance and `/undo` semantics are unchanged.

### Remote Computer backend

A `ComputerBackend` exposes screen capture and keyboard/mouse actions on a
remote endpoint. Capability: kind `computer`, owner `Session`, lifetime
`Run`/`Connection`, scope `Remote { endpoint }`, permission ceiling
`network_access` + `remote_side_effect`. Every action is a tool call with a
kernel call id: safety classifies it (remote side effects are always `Ask`
first), the resolver approves or denies, and the result becomes exactly one
terminal tool result. Cancellation propagates through the run token; frames and
screenshots are stored as artifacts and referenced by `MediaRef`, never inlined
into events. The backend cannot append events, resolve its own permissions, or
observe another session.

### Feishu/Slack/Telegram client adapter

An authenticated remote client connects with owner `Client(name)`, lifetime
`Connection`, and scope `Session`; it holds no tool-execution capability. It
can submit user turns (recorded as ordinary durable `UserMessage` with
normal provenance, exactly like TUI input or steering), request a bounded
semantic event feed, and answer permission prompts for sessions the operator
granted it. It cannot call tools directly, read canonical state outside the
event feed, or resolve permissions for another session. The adapter is a
steering surface, not a model tool: messages arrive as user intent and can be
handled by the existing steering/safe-boundary machinery, so append-only cache
epochs and tool-transaction atomicity are preserved.

### Dev server / TensorBoard / Jupyter / VNC exposure

A `ServiceProvider` exposes a host-managed service from a sandboxed process.
Capability: kind `service`, owner `Service(name)`, lifetime `Run`, scope
`Network { hosts: ["127.0.0.1"], ports: [6006] }`. The kernel records the
exposure as durable provenance, serves it through an authenticated channel,
and tears it down on run cancellation or session close. Raw socket passthrough
is not a capability; the declared hosts/ports bound what may be exposed, and
network access remains an `Ask` under Strict/Standard.

### MCP integration

An MCP server is connected by an operator-configured extension or remote host
and surfaces its tools through `ToolProvider`-equivalent registration. Its tool
calls flow through Latch's classification, permission, cancellation, and
result-integrity pipeline exactly like builtin tools. MCP resources can be
context sources. Latch's own extension protocol remains the internal plugin
ABI; MCP is an external integration layer, so replacing either one does not
disturb the other.

## 8. Implemented now vs deferred

| Concern | State |
|---|---|
| Capability vocabulary (`capability.rs`): kinds, descriptors, scope containment, handles, registry | **implemented**, with unit tests and an invariant test |
| `Agent::capabilities()` audit surface | **implemented** (read-only introspection) |
| `ContextEngine` port + `ContextRequest`/`ContextBudget`/`ContextView`; default `ContinuityEngine`; runtime-wide `ContextEngineFactory` for root and child sessions | **implemented**, behavior-preserving; port invariants in CI |
| Provider adapter boundary (`ModelProvider`) and tool boundary (`ToolExecutor`, extensions) | already existed; mapped, unchanged |
| Transport-agnostic framing (`FramedReader`/`FramedWriter` over generic `AsyncRead`/`AsyncWrite`) | already existed; documented as the transport seam |
| Remote extension transports, authentication, lifecycle | deferred |
| `WorkspaceBackend`, `ExecutorBackend`, `ComputerBackend`, `BrowserBackend`, `ServiceProvider` traits | deferred; first real implementation introduces its port |
| `Coordinator` / `PreferenceProvider` port traits | deferred; concrete supervisors and policy already exist |
| App protocol for remote clients (Feishu/Slack/Telegram) | deferred |
| MCP integration | deferred |

## 9. Where the invariants are enforced in code

| Invariant | Code |
|---|---|
| Session identity | `store.rs` (`create_session`), `agents/graph.rs`, provider session metadata |
| Event ordering / durability | `store.rs` append path; `tests/invariants.rs` |
| Transaction semantics | `store.rs` group claim transactions; `agents/group.rs` |
| Cancellation | `CancellationToken` propagated by `agent.rs`, tools, extensions |
| Permissions / sandbox | `safety.rs`, `permissions.rs`, `tools/policy.rs`, `sandbox.rs` |
| Evidence provenance | `agent/validation.rs`, `state.rs`, `agent/kernel_tools.rs` |
| Tool-call integrity | `agent/dispatch.rs`, `agent/request.rs` (`sanitize_tool_history`) |
| Secret isolation | `credentials.rs`, provider redaction |
| Context port invariants | `context.rs`, `continuity.rs` (default factory), `agents/supervisor.rs` (child policy), `tests/invariants.rs` (port parity, replaceability, child propagation, resume, fail-closed) |

Related documents: [`ARCHITECTURE.md`](ARCHITECTURE.md) for the full subsystem
map, [`CONTINUITY.md`](CONTINUITY.md) for the default context engine's memory
and cache semantics, [`PROTOCOL.md`](PROTOCOL.md) for the extension wire
format.
