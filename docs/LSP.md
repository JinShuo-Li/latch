# Lightweight language intelligence

## Design contract

Latch's LSP integration is a native, optional **read-only** navigation and
feedback workflow, not an embedded IDE. The kernel remains authoritative for
paths, permissions, execution, durable results and validation.

- One stable `lsp` tool: status, document symbols, definition, references, hover,
  diagnostics and stop. No raw RPC, shell command, rename or server-applied edits.
- Only operator-configured stdio servers; no downloads, repository-controlled
  executables, automatic package installation or new runtime prerequisite.
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
  documents are detected by bounded rereads. Results are advisory snapshots,
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

Implementation, configuration, limits and experimental evidence are documented
below as they are qualified.
