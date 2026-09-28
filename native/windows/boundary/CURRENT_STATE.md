# Windows workspace-read checkpoint (2026-09-28)

This is an **unfinished** checkpoint on `codex/windows-native-port`. The
workspace directory-enumeration regression and its recovery edge cases are
not yet qualified for release. The last fully green GitHub Actions run
(`36376680779`) predates these changes.

## Code in this checkpoint

- Workspace-read fixtures now cover exact file reads, workspace and nested
  directory enumeration, recursive `dir`/ripgrep, Git status, parent read and
  enumeration denial, and credential masking under the workspace.
- The metadata-only ancestor compatibility hook normalizes a trailing path
  separator. It does not grant directory listing to a workspace parent.
- Grant and rollback traversal exclude the trusted recovery journal when it
  lies inside a workspace. A newly created object with a NULL DACL has no ACE
  to remove, so rollback preserves its descriptor and scans its children.
- Diagnostics name the path when recovery cannot reopen a recorded object or
  inspect a new object's ACL. No test-only reconciliation override remains in
  source.

## Validation actually completed

- The native MSVC `/W4 /WX` build passed with the committed source, after
  removal of the local diagnostic override. `cargo fmt --all -- --check` and
  `git diff --check` also passed.
- The revised baseline `test.ps1` passed once with a small disposable
  workspace, including a recovery journal nested in that workspace.
- The expanded Rust test compiled with `--no-run`; it has not been executed.
  Full Rust Clippy passed before the final recovery edit and has not been
  rerun.
- The full native matrix, full Windows Rust gate, Linux Bubblewrap gate, and
  production release dogfood have **not** been rerun for this checkpoint.
  The earlier green CI run remains evidence for the preceding commit only.

## Local recovery blocker

An attempted real `%USERPROFILE%` read-only workspace contained about
625,000 files. Grant setup produced about 166,000 durable records and was
interrupted before child execution. The production recovery journal remains
at `%LOCALAPPDATA%\LatchBoundaryRecovery-v1\pending` on this host. The journal
is host state, not a repository artifact.

Recovery first refused changed identities of live Codex SQLite sidecars and
global-state files. A **local, test-only** reconciliation for direct regular
files under `%USERPROFILE%\.codex` let diagnostics continue; that bypass was
removed from source and is not part of this checkpoint. Recovery then found a
pre-existing NULL-DACL test file; the code now preserves that descriptor.
The next retry stopped while pinning
`%LOCALAPPDATA%\Microsoft\Windows\SFAP` with `ERROR_ACCESS_DENIED (5)`.
The pending journal still exists. The current state has **not** been proven
fully recovered. Do not delete or rename the journal: it records ACL changes
that may still need reconciliation. Do not start another native boundary
instance on this host until that recovery is resolved.

The requested smaller workspace `%USERPROFILE%\Desktop\work\school` exists
and contains about 577 files. It was inspected from the host, but **not**
tested through Latch, because the pending recovery blocks a valid sandbox
run. Resume with identity-checked recovery and a small, stable workspace;
avoid scanning the entire live home directory. Then rerun all gates and real
release-binary dogfood before calling the port ready.
