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

V0.1 declarations are a trusted cooperative auditing contract. An extension has
its operating-system user's authority. Latch does not claim seccomp, namespaces,
Landlock, WASM, or other syscall isolation. Configure only trusted executables.
