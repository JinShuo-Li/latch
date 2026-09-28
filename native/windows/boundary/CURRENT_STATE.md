# Windows workspace-read status (2026-09-28)

The workspace directory-enumeration regression is fixed and locally verified
on `codex/windows-native-port`. Linux Bubblewrap and Windows GitHub Actions
validation for this revision remain to be confirmed before release.

## Boundary behavior verified locally

- Workspace and nested directory enumeration, traversal, metadata and exact
  file reads work. `dir /a /s`, recursive ripgrep, and Git status work.
- A workspace parent remains metadata-only: directory enumeration and exact
  file reads outside the workspace are denied. Sensitive files under the
  workspace remain masked.
- A trusted recovery journal nested under the workspace is excluded from
  grants and rollback scans. NULL-DACL files retain their host descriptor.
- Before child execution, recovery scans only subtrees with a recorded ACL
  mutation or possible inheritance from one. After execution starts it scans
  every grant root to catch renames. It opens an unrecorded object read-only first and
  requests `WRITE_DAC` only when this transaction's ACE must be removed;
  identity and descriptor are checked again before that write. A focused
  crash fixture confirms an unrelated inaccessible host file is preserved.

## Completed local checks

- Native MSVC `/W4 /WX` build and all six native fixture suites: baseline,
  recovery, adversarial, lifecycle, deleted-root and developer tools. The
  adversarial fixture used `C:\project\latch\target` as its NTFS root;
  a separate attempt under `%LOCALAPPDATA%` failed its *host junction*
  precheck before the sandbox assertion.
- Windows `cargo fmt --all -- --check`, full workspace Clippy with
  `-D warnings`, `cargo test --workspace --locked` (serial test threads),
  and `cargo build --release --locked`.
- The real release `latch.exe` completed a mock-provider coding task in a
  disposable Git repository: read, search, write, Git status, validation,
  managed-process start/poll/terminate, and revalidation; final task state
  was `verified`.
- The real release binary completed a read-only task in
  `%USERPROFILE%\Desktop\work\school` (about 577 files): read, search and
  shell listing all succeeded. Direct native tests also passed exact read,
  recursive listing and `rg --files`, while parent enumeration was denied.

## Recovered local transaction and remaining limit

An earlier whole-`%USERPROFILE%` probe encountered about 625,000 files and
was interrupted after about 166,000 durable ACL records. Recovery initially
refused changed identities of live Codex SQLite sidecars and state files. A
**one-time local, test-only** reconciliation of direct regular files under
`%USERPROFILE%\.codex` allowed identity-checked recovery to continue; that
exception was removed from source before the current build and tests.
Production recovery still fails closed on an unrecognized replacement.

The improved scanner preserved unrelated NULL-DACL and access-denied host
files. Recovery exited successfully, removed the production `pending`
journal, and a second check found no running owner or retained journal. No
journal was manually deleted. The large live home directory remains a
performance and churn limit for per-object grants; it is **not** counted as
successful whole-home dogfood. Use a smaller stable workspace such as
`Desktop\work\school` until that scale case is designed and qualified.

The last previously green full GitHub Actions run (`36376680779`) predates
these changes. Do not call this revision fully qualified until the new full
Windows native and Linux Bubblewrap CI run passes.
