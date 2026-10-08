# Windows workspace-read status (2026-09-28)

The workspace directory-enumeration regression is fixed for grantable NTFS
trees on `windows-native-port`. Full GitHub Actions validation passed
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

The earlier full validation run used a temporary push trigger to exercise its
branch while the `workflow_dispatch` workflow was absent from the default
branch. That trigger was removed after the green run; default push CI remains
light. At that point, the final follow-up commit changed only workflow
triggering and documentation.

## Large-workspace design assessment

The per-command boundary traverses each workspace, write and read root once to
validate hardlink aliases and records file identities and relative paths in a
compact plan. It
does not retain a handle for every object. Before applying each ACL batch, it
reopens the target through checked path components, compares its NTFS identity,
and checks hardlinks again; at most 32 object handles are held for a batch.
Workspace and write-root changes grant both the AppContainer SID and
write-restrictor SID; read-only roots grant the AppContainer SID. All retain
one exact recovery intent and ACL mutation per object, with up to 32 intents
published in one bounded, checksummed record before those mutations begin.
Sensitive path exclusions and rollback still add per-object work. A root-only inherited ACE is not a safe
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

Multiple threads are not the first optimization. Each ACL mutation still
requires a durable exact intent and a verified write, while rollback checks
the entire transaction before restoring it. Parallel ACL mutation would
require a thread-safe, ordered journal and new crash/recovery proofs; it would
also increase contention and in-flight host changes. Measure scan, journal,
grant, and rollback time separately before considering that change.

The earlier paired-SID grant change passed the local MSVC `/W4 /WX` native
build, all six native suites (including exact restoration of a protected
workspace child ACL), Windows formatting, full workspace Clippy and tests, and
a locked release build. The current compact-plan, identity-revalidation,
batched-journal changes and new fixtures have not run on Windows yet. Journal
batching reduces write-through flushes but keeps the per-object intent and ACL
mutation. No whole-home startup timing or whole-home success is claimed.

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

A local clone of `windows-native-port` made with `git clone --no-hardlinks`
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

## Follow-up fixtures (2026-10-08)

The native runner now supports opt-in phase timings with
`LATCH_BOUNDARY_TIMING=1`: grant-root preflight scan, grant traversal, durable
journal writes, ACL application, and rollback are reported independently.
For workspace, write and read roots, preflight stores compact NTFS identities
and a flat arena of relative paths instead of keeping every file handle open.
All roots complete
hardlink preflight before any grant. The grant pass reopens and revalidates each
object, while holding no more than 32 target handles per ACL batch. Each batch
is durably published before changes are applied; recovery accepts either the
original or granted descriptor for every entry in an interrupted batch. The
existing per-object intent and ACL update remain in place, while journal
flushes are grouped. Recovery also preflights journaled file identities one at
a time, then reopens and rechecks each ACL immediately before restoration.
During descendant cleanup it retains handles only for changed directories,
which can pass temporary inherited ACEs to new children; it does not keep a
handle open for every file in a large workspace.
`native/windows/boundary/scale.ps1` adds a synthetic 4,096-file workspace case
that creates host files while grants are live, mutates files inside the
AppContainer, and checks exact restoration for pre-existing objects plus grant
removal from new objects. It is wired into manual Full validation. These new
measurements and acceptance checks have not been run in this environment, and
they do not change the existing whole-home support limit or implement a grant
lease.

`native/windows/boundary/network.ps1` now specifies the loopback policy through
client and listener fixtures. It probes an external DNS name when the host can
resolve it, `localhost` resolution, IPv4, available IPv6, and accepts an
optional reachable private-LAN endpoint for separate capability
qualification. Sandbox commands retain Windows' default
loopback isolation; the runner does not edit the AppContainer exemption list
or firewall. The documented Windows API is a debugging exemption list whose
existing entries must be preserved, and the documented inbound development
path remains active while the listener is running. A narrow local endpoint
relay remains a separate design. The fixture has not yet been run here.

`lifecycle.ps1` probes an x86 top-level command and a direct x86 child on
WOW64 hosts. A started child must remain in the AppContainer job; an explicit
creation refusal is accepted and cleanup is checked in either case. Deeper x86
descendants and descendants that bypass compatibility injection remain
unqualified. Python and Windows PowerShell 5.1 extension round-trip tests are
now in the Windows Full validation matrix; neither has run here. Physical
power-loss recovery and hostile rename/reparse/hardlink races remain open.
