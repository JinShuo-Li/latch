# Native Windows boundary checkpoint — 2026-09-27

This is unfinished experimental code on `codex/windows-native-port`. The
maintainer requested scoped bug fixes, a committed record of the remaining
tests, and a pushed checkpoint. It is **not a completed Windows port**.
Production `ExecutionBackend::detect` on Windows still refuses execution.
Version remains 0.2.3. Do not enable this candidate or merge it into main as a
working Windows backend based on the checks below.

## Implemented candidate

- Per-call AppContainer plus a separate, unique WRITE_RESTRICTED SID. Neither
  the All Application Packages SID nor a broad host group is a write restrictor.
- Explicit NTFS workspace/external grants. Write grants exclude WRITE_DAC,
  WRITE_OWNER and FILE_DELETE_CHILD. Git denies contain mutation bits only:
  denying SYNCHRONIZE would accidentally deny ordinary reads.
- Existing hardlinks are enumerated before write grants; aliases outside the
  approved roots cause refusal before any workspace grant. Handles without
  delete sharing pin scanned objects through grant setup.
- Sensitive trees have package-authority allow ACEs removed and inheritance
  sealed, preserving ordinary host ACEs. This persists on the supplied
  sensitive paths. Per-package deny ACEs alone were insufficient against
  All Application Packages read grants, including on protected child DACLs.
  This behavior needs a production compatibility/security review.
- A private desktop on WinSta0 permits USER32/GDI startup without granting a
  broad restricting SID or creating a private window station.
- A small Detours compatibility DLL forwards only NUL/KsecDD device handles
  and injects compatibility support into descendants. These hooks are not
  the security boundary; restrictions and job membership are OS properties.
- Explicit inherited standard-handle allowlist, and atomic creation inside
  a kill-on-close Job Object. A separate trusted cleanup owner holds the
  job, profile and ACL grants. It watches a synchronization-only handle to
  the public launcher; launcher termination stops the entire sandbox job,
  waits for every process to exit, then revokes grants and deletes the profile.
  Error paths drain the job before unwinding into ACL cleanup as well.
- Missing top-level .git is reserved by a temporary delete-on-close file.
- Native cmd.exe shell and direct executable/argv inspection are exercised
  by a Rust test. Git Bash/MSYS is not supported by this candidate.
- Native helpers build from the Windows Cargo build script with MSVC and are
  embedded in a test-only runtime module. No unsafe Rust was introduced.
- No account provisioning, firewall changes, WSL, or unrestricted retry.

## Bugs fixed in this checkpoint

Outside-write escape through pre-existing workspace hardlinks; credential reads
through protected child DACLs and package allow grants; accidental Git read
denial from generic-write deny masks; a create/assign Job Object race; Cargo
child standard-handle inheritance; Cargo jobserver access to its own isolated
object namespace; linker access to private AppContainer temp storage; cmd.exe
extended/mixed path handling; compatibility handle cleanup on failed startup;
and stale .git reservation cleanup on runner death.

Git inspection and extension hosts now use the fixed-program execution entry
point. Linux still uses mandatory Bubblewrap and explicitly execs that fixed
program. Windows production entry points continue to refuse execution.

## Verified with real native subprocesses on this Windows 11/NTFS host

- 18 baseline assertions: workspace reads; denied read-only write/create;
  allowed workspace write/create; state read/write/ACL denial; ordinary
  outside reads; outside write/ACL denial despite broad Users/package grants;
  external-root denial/grant; Git read/write/delete/ACL behavior.
- Adversarial assertions: separate WRITE_DAC and WRITE_OWNER denial, outside
  NULL-DACL write denial, protected-child credential read denial, outside
  writes through a junction, outside hardlink refusal without ACL change,
  allowed internal hardlinks, and missing .git creation denial.
- Four-generation trees: timeout (exit 124), root process exit, and runner
  termination. All descendants disappear, and the missing-.git reservation
  disappears. These are native runner checks, not Latch managed-tool tests.
- A dependency-free Cargo fixture compiled, linked and passed its unit test
  and rustdoc phase inside the boundary using the MSVC developer environment.
- Rust embedded-runner test: native shell read, denied write, and Git version.
- SeDebugPrivilege cannot be enabled; HKLM SYSTEM cannot be opened for writes.
- TCP handshake to example.com: ordinary host baseline succeeded; no network
  capability returned WSAEACCES (10013); explicit internet/private-network
  capabilities succeeded. No application payload was sent.
- Loopback connections timed out both with and without network capabilities.
  AppContainer loopback exemptions were not configured. Do not claim localhost
  development-server connectivity works.
- Final cleanup verification: native C++ compilation with /W4 /WX;
  `cargo fmt --all -- --check`; `git diff --check`;
  `cargo clippy -p latch-kernel --all-targets --locked -- -D warnings`;
  the embedded-runner Rust test; and all three native fixture scripts passed.
  The Rust test took 58.11 seconds on this host; per-call AppContainer profile
  lifecycle overhead remains a performance concern.

Local disposable evidence directories are under `C:/project/latch/target/`:
`boundary-matrix-03`, `boundary-adversarial-08`,
`boundary-lifecycle-01`, `boundary-network-01`, and
`windows-port-probes/cargo-boundary.log`. They are intentionally not committed.
Final native matrix logs are in `windows-port-final/{test,adversarial,lifecycle}.log`.

## Cleanup follow-up (2026-09-27)

The public launcher now starts a trusted cleanup owner from the same executable.
Only that owner holds the job and per-call resources. No model command is run
outside the restricted AppContainer. The owner receives an explicit inherited
launcher-lifetime handle and standard handles; the lifetime handle is excluded
from sandboxed children. It observes launcher exit during filesystem setup and
waits on launcher/target exit while the command runs.

This fixes retained grants/profiles after killing the public launcher. Grant
records are allocated before ACL mutation, process attribute lists use RAII,
and ACL/profile cleanup failures make the command fail (125). If job teardown
cannot be confirmed, the owner exits without attempting ACL revocation; closing
its job handle still requests process termination. This is not a crash journal.

Verified again with real native subprocesses: all 18 baseline file assertions;
all adversarial assertions; timeout, root-exit and launcher-kill of four-generation
trees; exact restoration of workspace/runtime ACLs and removal of the temporary
.git marker and observed AppContainer profile; and cleanup after a missing
executable fails to start. Fixture targets record their actual AppContainer SID
so checking cleanup does not race short-lived targets. C++ /W4 /WX, workspace
format checking, and kernel Clippy with all targets/features also pass. The
embedded-runner Rust test passed again (one test, 375 filtered; 48.64 seconds).
A fresh dependency-free Cargo build in workspace/target-owner-followup also
compiled, linked and passed its unit test and rustdoc phase inside the boundary.

Evidence directories for this follow-up: `boundary-owner-matrix-02`,
`boundary-owner-adversarial-02`, and `boundary-owner-lifecycle-06` under
`C:/project/latch/target/`. Killing the cleanup owner itself, machine crashes,
cancellation at every startup phase, and concurrent ACL mutation are still
unverified; killing both trusted processes can still leave temporary grants
and profiles behind. Production detection remains disabled.

## Reproduce focused checks

Use native PowerShell as an ordinary user. The outer Codex restricted tool
token cannot stand in for the host user when testing this nested boundary.
No elevation to administrator is required.

Prerequisites: x64 MSVC Rust, Visual Studio C++ Build Tools and Windows SDK,
CMake 3.25+, Git for Windows. Run CMake from an x64 developer command prompt:

```text
cmake -S native/windows/boundary -B target/windows-boundary -G "NMake Makefiles" -DCMAKE_BUILD_TYPE=Release
cmake --build target/windows-boundary
cargo test -p latch-kernel --lib native_shell_and_fixed_git_use_embedded_boundary --locked -- --nocapture
cargo clippy -p latch-kernel --all-targets --locked -- -D warnings
```

Run `test.ps1`, `adversarial.ps1`, and `lifecycle.ps1` in this directory,
each with `-Binaries <absolute build/bin>` and
`-FixtureRoot <new absolute disposable directory>`.
The scripts refuse an existing fixture directory. Fixtures contain synthetic
credentials only; do not point them at actual credential or state directories.

## Not verified / not finished

- **Full Windows gates:** workspace fmt/clippy/test/release combination;
  all Windows test failures, CLI/TUI suites, and installation/package behavior.
- **Linux:** the local Bubblewrap release gate has not run on this Windows
  host. The previous checkpoint (730dbdd) passed the full Ubuntu CI release
  gate in GitHub Actions run 36257342807. Follow-up commits require their own
  CI run; no Linux behavior or security relaxation is intended.
- **CI:** no Windows CI workflow has been added. Existing Linux CI remains.
- **Real dogfood:** no native Latch NTFS coding task; no live/model agent proof
  of inspection, editing, search, validation, Git and managed tools together.
- **Integration:** production detection, doctor, model shell instructions,
  all CLI Git inspection paths, search, validation, extension fixtures and
  exec_start/exec_poll/exec_terminate/cancellation/drop through Latch itself.
- **Filesystem adversaries:** concurrent grants and revocations, crash
  recovery/ACL journals, hostile rename/reparse/hardlink races, all existing
  open-handle races, nested Git repositories, worktree/common-dir cases,
  alternate streams, Unicode/long paths, ACL size limits, network filesystems,
  and conflicts with unrelated host programs.
- **Credentials:** Windows Credential Manager/DPAPI and IPC routes; runtime
  protection against hostile peers; future protected files with explicit
  package grants; complete credential-location coverage and real-home impact
  of persistent ACL hardening. NULL/unsupported sensitive DACLs fail closed.
- **Processes:** Latch managed-process durability and cancellation paths;
  forced runner death at every startup phase; all handle inheritance modes;
  32-bit children; descendants bypassing compatibility injection; resource
  exhaustion; recovery after cleanup-owner termination or machine crashes.
  Public-launcher termination cleanup is covered by the follow-up tests.
- **Network:** DNS, UDP, IPv6, private LAN, listening servers, proxies, named
  pipes and other IPC. TCP evidence is limited to the tested endpoint and host.
- **Compatibility:** PowerShell, Python extensions, npm workflows, full
  repository Cargo builds under the boundary, VS discovery without an
  explicit developer environment, non-English/Unicode runtime installation
  paths, and KsecDD handle exposure review.
- **Release:** README installation instructions, doctor, security/runtime docs
  for an enabled backend, Windows packaging, version bump and release build.

The kernel integration module is deliberately `cfg(all(windows, test))`.
Production Windows execution is still fail-closed, including fixed commands.
