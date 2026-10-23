# Git Bash as a shell, not as a sandbox boundary

Date: 2026-10-23
Status: design note for the `windows-backend-prototype` branch. This is not an
implementation plan for merging that branch.

## Summary

The prototype's use of Git for Windows Bash is directionally right for the
model-facing shell and directionally wrong for the security boundary. Keep the
POSIX-shell idea; do not keep the write-restricted-token/MSYS/Detours
foundation. Re-establish the idea as a narrow feasibility spike on top of
current `main`, whose native AppContainer boundary already runs `cmd.exe`.

## What each branch does

`main` (v0.3.2 and later) installs an embedded native Windows runner. Each call
gets an AppContainer identity, a separate unique `WRITE_RESTRICTED` SID, a
kill-on-close Job Object, and mediated read/write/socket brokers. USER32/GDI
startup uses a private desktop on the launcher's current window station. The
model-facing shell is native `cmd.exe`; `execution/shell.rs` owns the cmd
dialect and its conservative read-only grammar. The documented position is
explicit: there is no Git Bash, WSL, local-account, or unsandboxed fallback.
The same file shows the cost of that choice: Windows validation refuses a
pipeline because `cmd.exe` has no `pipefail`, and the read-only grammar has to
compensate for cmd expansion and path quirks.

`windows-backend-prototype` discovers Git for Windows `bin/bash.exe`, starts it
under a per-call `WRITE_RESTRICTED` token with a unique SID, journals ACL
grants, uses an MSYS guard and a Detours hook, and contains descendants in a
Job Object. Its private-desktop experiment did not establish a boundary:
`CreateWindowStationW` returned Win32 error 5 even for a direct native control
program without the Latch token. The branch's own experiment note already says
the diagnostic prototype is not a verified secure backend and must not merge.

## Why Git Bash makes sense as a shell

- The coding model and the tool contract are optimized for POSIX shell
  semantics. Pipes, quoting, command substitution, heredocs, and `set -o
  pipefail` are part of normal expectations on Linux.
- `cmd.exe` forces the model to translate paths, variables, redirection, and
  command chaining per platform. That is a constant reliability tax, not a
  one-time adaptation.
- `cmd.exe` cannot prove upstream success in a pipeline. `main` therefore
  rejects Windows validation pipelines outright, which removes a common way to
  run tests and linters.
- Git for Windows is already a normal Windows prerequisite for Git, and
  `bash.exe` normally ships with it. The shell is therefore familiar to users
  even though `main` deliberately does not depend on it.
- `main` already separates shell syntax (`execution/shell.rs`) from execution
  (`ExecutionBackend`). A POSIX dialect can be added as a shell choice without
  changing the capability model.

## Why it does not make sense as the boundary

- MSYS2/Git Bash is a large dynamic runtime with process and fork emulation,
  named-object IPC, a private `/tmp`, and a wide DLL surface. Its startup and
  child behavior are difficult to reconcile with a strong kernel-enforced
  boundary.
- The prototype's answer was a write-restricted token plus ACL grants, an MSYS
  guard, and a Detours hook. Hooking compatibility into the loader is not a
  security boundary. `main`'s own boundary documentation says the same about
  its compatibility DLL: restrictions and Job membership are the OS
  properties; hooks are not.
- The prototype's private-desktop attempt failed before the restricted child
  started, and the broad-restrictor alternative retained a known outside-write
  escape. Neither result is evidence that Git Bash itself is unusable, but
  together they rule out merging that implementation as a secure backend.
- Requiring Git Bash for the sandbox would also regress the current binary
  install story, where `latch.exe` embeds the native boundary and does not need
  Bash. Discovery could remain optional, but it must not be the only path.

The distinction matters: the prototype conflated "the shell the model talks
to" with "the mechanism that enforces capabilities". The first is a product and
reliability decision. The second is a security decision.

## Branch mechanics

Do not merge `main` into this branch, and do not cherry-pick the prototype onto
`main`.

The merge base is `ebdc9f2`. As of this note, `main` is 221 commits ahead and
this branch is 5 commits ahead. `main` has rewritten the Windows execution
layer (`execution/shell.rs`, `execution/windows_runtime.rs`,
`execution/windows_runtime_tools.rs`, and `native/windows/boundary/*`). The
prototype's monolithic `execution/windows.rs` and its MSYS guard no longer map
onto that structure. A merge would produce a large, misleading conflict
resolution rather than a reviewable change.

## Recommendation

1. Archive `windows-backend-prototype` as an experiment. Keep its written
   findings and the reusable primitives, not its backend.
2. Start a fresh, narrow spike from current `main` that answers one question:
   can `bash.exe` run inside the existing AppContainer profile and broker
   boundary without weakening either?
3. Make the spike prove a small matrix before any design work: start Bash,
   run `printf`, `rg`, and `git`, read and write workspace files, use the
   scratch directory, start children, and terminate the whole Job.
4. If the matrix passes, add a POSIX shell dialect behind
   `execution/shell.rs`. `ExecutionBackend` stays the AppContainer runtime; the
   shell is selected, not substituted for the boundary.
5. If the matrix fails, record the concrete blocker (named objects, temp,
   desktop, GDI, handle inheritance, or fork behavior) and stop. The fallback
   is better `cmd.exe`/PowerShell ergonomics and argv-first fixed commands, not
   a weaker token sandbox.

Reusable prototype assets, if needed:

- strict Git for Windows layout validation and the `bash.exe` startup probe;
- `bash_quote` and its escaping test;
- reparse-point checks for workspace, scratch, and runtime directories;
- hash-addressed, read-only materialization of embedded native artifacts.

Do not reuse the restricted-token backend, the MSYS IPC guard, or the Detours
injection path as the security boundary.

## Questions the spike must answer

- Does `bash.exe` start under the current AppContainer profile without adding
  grants outside the existing workspace, scratch, and read roots?
- Which writes does MSYS require outside the workspace (BaseNamedObjects,
  `/tmp`, profile state), and can the existing brokers or a bounded grant
  cover them without weakening the boundary?
- Does MSYS fork emulation survive the Job Object, handle allowlist, and
  AppContainer token?
- Do `rg`, `git`, Cargo, Node, and Python keep working through the brokers when
  launched from Bash rather than cmd?
- What happens on a minimal Git install where `bash.exe` is absent? Optional
  discovery must fail with an actionable message, not a broken default.
- If a shell is bundled instead of discovered, what are the licensing,
  signing, and update obligations?

## Non-goals

- Replacing AppContainer with a write-restricted token as the security model.
- Treating a Detours hook or an MSYS guard as the boundary.
- Requiring Git Bash for binary installs that currently need only Git and
  ripgrep.
- Merging `windows-backend-prototype` as-is.

## Bottom line

The prototype's instinct about Git Bash is worth keeping. The prototype's
implementation is not. The defensible direction is **main's boundary plus an
optional POSIX shell**, validated by a small spike, with `cmd.exe` remaining
the fallback when Bash is unavailable.
