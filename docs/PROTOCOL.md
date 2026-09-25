# Extension protocol 0.1

Extensions use JSON-RPC 2.0 over stdin/stdout. UTF-8 JSON is LSP-framed:

```text
Content-Length: <bytes>\r\n
\r\n
<JSON bytes>
```

Frames are limited to 16 MiB. Stdout is protocol-only; diagnostics use stderr.
The host sends `initialize` with the protocol version, client, workspace, and
`permissionContract: cooperative-audit`. The extension must return the exact
version. The host then sends `initialized`; registration ends with `ready`.

| Method | Required field | Meaning |
|---|---|---|
| `tool.register` | `name` | Add a tool with description and input schema |
| `command.register` | `name` | Add a command |
| `hook.observe` | `event` | Observe eligible lifecycle events |
| `hook.transform` | `structure` | Transform one authorized structure |
| `hook.guard` | `action` | Provide allow/deny/ask decisions |
| `context_source.register` | `name` | Add a context source |

Invocation uses `tool.execute` with `{name, arguments}`. Shutdown is a `shutdown`
request followed by `exit`. Observe, transform, and guard remain separate; there
is no universal hook.

Every lifecycle stage is bounded by the central, configurable
`[extension_lifecycle]` policy: process spawn plus the `initialize` request
write, the `initialize` response, registration/ready collection, each ordinary
RPC response, the `shutdown` response, and graceful exit after `exit`. A missed
deadline names the extension and stage, kills the child, and reaps it; startup
also observes the cancellation token, so Ctrl+C cannot be pinned by a silent
extension.

Extension declarations remain a cooperative auditing contract, but v0.2.1
changes the process boundary: Latch starts the extension host inside the same
mandatory Bubblewrap sandbox as every other command, with a read-only
workspace, masked home credentials and sockets, and network access for protocol
work. The host cannot write project files or read masked host secrets. Since
v0.2.2, the configured Latch state directory and resolved credential target
are masked as well. What is
still cooperative is the extension's own tool behavior: Latch does not classify
individual extension tool arguments as separate capabilities, and there is no
seccomp, Landlock, or WASM isolation inside the host. Configure only trusted
executables.
