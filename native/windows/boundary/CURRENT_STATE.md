# Windows workspace-read status (2026-09-28)

The workspace directory-enumeration regression is fixed for grantable NTFS
trees on `codex/windows-native-port`. Full GitHub Actions validation passed
for commit `1d14805` (run `36409963890`): Windows native, Windows Rust, and
Linux Bubblewrap release jobs all succeeded. The lightweight CI run
`36409875345` also passed on Windows and Linux.

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

The full validation run used a temporary push trigger to exercise this branch
while the `workflow_dispatch` workflow was absent from the default branch.
That trigger was removed after the green run; default push CI remains light.
The final follow-up commit changes only workflow triggering and documentation.

## Large-workspace design assessment

The per-command boundary traverses each write root to validate hardlink
aliases, then grants and journals ACL changes on existing objects for both the
AppContainer SID and the write-restrictor SID. Shared grant roots now apply both
SIDs in one ACL mutation and one durable intent per object, avoiding a second
grant walk and flush. Sensitive-path exclusions and rollback still add per-object
work. A root-only inherited ACE is not a safe
drop-in optimization: Windows can propagate it to existing children, while
protected or explicit child ACLs and sensitive exclusions still need individual
handling. The current recovery journal must be able to undo every changed ACL
after a crash. The whole-home probe above demonstrates that this work can
dominate startup and that a live, changing root can also block recovery.

A scalable implementation needs a separately qualified workspace grant lease
with a stable, narrowly scoped AppContainer identity: reuse verified grants
across commands, keep write-root hardlink checks and
sensitive exclusions, record object identities and ACL changes durably, and
revoke or recover the lease on normal exit, cancellation, and crash. It also
needs an explicit policy for host changes during the lease. Until those rules
and adversarial fixtures exist, use focused, stable workspaces rather than an
entire live home directory. This is a known scale limit, not a passed
whole-home test.

Multiple threads are not the first optimization. The hardlink preflight is
read-only and could use bounded parallelism after profiling, but it currently
pins every object until grants finish. Each ACL mutation then writes and flushes
an ordered recovery intent before changing the object. Parallel ACL mutation
would require a thread-safe, ordered journal and new crash/recovery proofs; it
would also increase contention and in-flight host changes. Measure scan,
journal, grant, and rollback time separately before considering that change.

The paired-grant change passed the local MSVC `/W4 /WX` native build, all six
native suites (including exact restoration of a protected workspace child ACL),
Windows formatting, full workspace Clippy and tests, and a locked release build.
It reduces grant walks and durable ACL intents on shared roots; no whole-home
startup timing or whole-home success is claimed.

## Existing-workspace ACL blocker (2026-09-28)

The installed production runner still refuses `C:\project` even after its old
`target` trees were cleaned (about 190,800 files removed). The remaining tree
has about 630 files and no host-inaccessible directory. A read-mode `dir /a`
fails closed with `pin recovery object: 5` at
`C:\project\latch\.worktrees\windows-native\.latch`; its owner is
`CodexSandboxOffline`, while the current user has Modify but not `WRITE_DAC`.
Many source files have that foreign owner. The runner cannot add its unique
AppContainer and write-restrictor ACEs to those objects. No production recovery
journal remained after the refusal. `C:\project` itself is also not a Git
repository, so `git_status` cannot succeed with that workspace selection.
The inaccessible old test fixture was preserved intact under
`%LOCALAPPDATA%\LatchBoundaryFixtureQuarantine`, outside the workspace.

A local clone of `codex/windows-native-port` made with `git clone --no-hardlinks`
at `Desktop\work\latch-native-port` is owned by the current user. The same
installed production runner completed directory listing, ripgrep and Git
status there, then removed its recovery journal. This is an operational
workaround, not support for foreign-owned files in the original workspace.
An end-to-end run of the installed `latch.exe` against a local mock provider
also returned `ok` for its `shell`, `search`, and `git_status` tools in that
clone; no real provider or credential was used.
Supporting arbitrary host-readable but ACL-unmodifiable objects needs a
separately verified read broker or other Windows boundary design; changing
their owners or granting broad host ACLs is not an acceptable automatic fix.
