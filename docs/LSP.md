# Lightweight language intelligence

## Design contract

Latch's LSP integration is a native, optional **read-only** navigation and
feedback workflow, not an embedded IDE. The kernel remains authoritative for
paths, permissions, execution, durable results and validation.

- One stable `lsp` tool for configured sessions: status, document symbols,
  definition, references, hover, diagnostics and stop. No raw RPC, shell command,
  rename or server-applied edits. Unconfigured sessions have no extra tool schema
  or prompt context; startup/stop never changes a configured session's schema.
- Only operator-configured stdio servers; no downloads, automatic execution of
  repository-provided commands, package installation or new runtime prerequisite.
- Lazy startup against the fixed launch workspace. No server exists until a
  semantic query needs it. Unix sessions reuse clients; child sessions own
  independent clients. Windows uses per-query clients to release the native
  recovery lock between tools, as MCP does.
- Servers run through `ExecutionBackend` with read-only workspace access,
  private writable scratch, masked secrets and **no network**. Strict safety
  requires approval before executing a language server. Server requests cannot
  authorize commands, edits or wider capabilities.
- Full-text open-document synchronization before queries. Durable workspace
  mutations invalidate the cached project client; external changes to opened
  documents are detected by bounded rereads and feed the existing observation
  ledger so previous validation becomes stale when drift is detected. Results are advisory snapshots,
  never a claim of complete indexing or a substitute for `validate`.
- Input/output, documents, frame headers, server requests, results, startup,
  queries, lock waits, cancellation and teardown are bounded. Failed or cancelled
  clients are discarded, not silently reused or automatically retried.
- Tool results persist through the existing tool event lifecycle. LSP process
  state is disposable and reconstructed lazily after resume; it never enters
  canonical memory, provider serialization or the stable system prompt.

The first implementation intentionally omits global filesystem watching,
workspace edits, completion menus, remote LSP and automatic build permissions.
An unopened file changed by an external editor may depend on server-specific
watching; LSP results are not proof of whole-workspace freshness. Builds and tests
remain required for certification.

## Configuration

Servers are optional and must already be installed. Nothing is bundled or
installed by Latch. For example, after installing clangd yourself:

```toml
[[lsp_servers]]
name = "clangd"
command = "clangd"
args = ["--background-index=false", "--clang-tidy=false", "--enable-config=false"]
extensions = ["cpp", "hpp"]
language_id = "cpp"
timeout_seconds = 30
```

The deliberately conservative example disables background indexing and project
configuration: it is useful for loaded-file navigation, not a guarantee of
complete cross-project references. Indexing and compiler configuration remain
server-specific. The launch workspace is the fixed LSP root; Latch never changes
roots based on server requests. A monorepo may need server-specific configuration.

A Rust configuration can use an independently installed `rust-analyzer`:

```toml
[[lsp_servers]]
name = "rust"
command = "rust-analyzer"
extensions = ["rs"]
language_id = "rust"
[lsp_servers.initialization_options]
checkOnSave = false
[lsp_servers.initialization_options.cargo.buildScripts]
enable = false
[lsp_servers.initialization_options.procMacro]
enable = false
```

The Rust example is configuration guidance, not a native Rust-server qualification:
toolchain shims may exist without the actual component installed. LSP never
implicitly grants workspace/build writes or networking to make a server work.
A server requiring those privileges may fail; ordinary search/read/build tools
remain available through their existing policies. No unsandboxed fallback exists.

`enabled` defaults to true; `args`, `initialization_options` and `settings` are
optional. `settings` answers `workspace/configuration` requests and is also sent
as `workspace/didChangeConfiguration`. These are operator settings, not permissions.
At most eight servers may be configured; enabled extension mappings must be
unambiguous (case insensitive, no leading dots). Configuration is loaded at session
construction, including children and resume; edit it and start/reconstruct a
session to change the server catalog.

## Model workflow

The session context lists available languages and recommends semantic navigation.
The optional schema adds fewer than 400 estimated tokens; schemas do not change
as clients start, fail or stop. Navigation examples:

```json
{"action":"symbols","path":"src/main.cpp"}
{"action":"definition","path":"src/main.cpp","line":12,"symbol":"quote"}
{"action":"references","path":"src/pricing.cpp","line":8,"symbol":"quote"}
{"action":"hover","path":"src/main.cpp","line":12,"symbol":"quote"}
{"action":"diagnostics","path":"src/main.cpp"}
```

`line` and `column` are **1-based**, with columns measured in **UTF-16 code units**,
not terminal cells or UTF-8 bytes. Prefer `symbol`, an exact unique substring on
the line, instead of computing a column. Ambiguous substrings and positions inside
a surrogate pair are rejected. Returned ranges use the same convention; end
positions are exclusive. Locations outside the workspace, protected state and
file aliases/hardlinks are omitted. Use `read_file` to inspect a returned location
before a guarded edit; LSP does not supply a write authorization or base hash.

A useful loop is **navigate → inspect → edit → diagnostics → validate**. Snapshot
checks cover all opened documents; changes during a query fail or mark its result
stale. Changing an imported opened buffer also re-versions unchanged buffers,
preventing queued old versioned diagnostic reports from looking current.

Diagnostics use pull when advertised, otherwise push. Versioned reports must
match the document snapshot. Unversioned reports are explicitly tagged
`push-unversioned-freshness-not-proven`; no error list, empty result or progress
notification certifies complete indexing or correctness. A push-only server that
never publishes diagnostics times out explicitly rather than fabricating success.

Server `workspace/applyEdit` requests are refused, command execution and dynamic
registration requests are unsupported, and workspace folder replies never widen
scope. Arbitrary server metadata/commands never enter model tools or task state.
Unavailable servers produce an actionable tool failure; the model should use
`search`/`read_file`, not repeatedly retry LSP or install software without approval.

## Resource and lifecycle limits

| Resource | Limit |
| --- | --- |
| Active clients | One per session; language switches close the previous client |
| Open documents | Eight, each at most 1 MiB of UTF-8 text |
| JSON-RPC frame / header | 4 MiB / 8 KiB; duplicate lengths rejected |
| Final tool output | 24 KiB; at most 50 displayed results, with truncation metadata |
| Notifications/server requests | At most 2048 per response wait |
| Query including lock wait, startup and synchronization | 30s default, configurable 1–300s |
| Shutdown | Bounded graceful request, then platform process-tree termination |

There is no background watcher, index database, task or polling loop inside
Latch. Idle Unix servers may retain their own indexes until stop/session teardown;
only their stdio and bounded client buffers are retained by Latch. Cancellation or
query failure discards the client. Windows restarts per query because keeping a
native boundary transaction alive would block other tools. This is a deliberate
performance tradeoff; Windows stateful/large-index LSP workloads remain unqualified.

## Validation

```sh
cargo test -p latch-kernel --lib lsp --locked
# Optional local server qualification; clangd and a C++ compiler required:
cargo test -p latch-kernel --lib real_clangd --locked -- --ignored --nocapture
```

The deterministic Python peer checks framing, sandbox write denial, refused server
edits/commands, external changes, versions, workspace mutation invalidation,
independent children, persistent results, cancellation, deadlines, pull/push
reports and fallback. The real clangd fixture distinguishes overloaded functions
and refreshes diagnostics. Neither requires model credentials.
