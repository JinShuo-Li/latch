# Native Windows boundary — 2026-09-27

`windows-native-port` selects this runner from the production
`ExecutionBackend`. Version remains 0.2.3. Native fixture results below
establish the listed cases only; do not merge a red Windows gate into main.

Current validation status and the large home-directory performance limit are
recorded in [CURRENT_STATE.md](CURRENT_STATE.md). Its more recent checks take
precedence over older validation results below.

## Implemented candidate

- Per-call AppContainer plus a separate, unique WRITE_RESTRICTED SID. Neither
  the All Application Packages SID nor a broad host group is a write restrictor.
- Production workspace/external writes use `write_broker.{h,cpp}` with pinned
  parent/root identities, hardlink checks and scoped file capabilities. No
  source ACL changes or WRITE_DAC are needed; rename/link are mediated and
  directory namespace/ACL/owner rights are withheld.
- Legacy native fixture backend: explicit NTFS workspace/external grants. Write grants exclude WRITE_DAC,
  WRITE_OWNER and FILE_DELETE_CHILD. Git denies contain mutation bits only:
  denying SYNCHRONIZE would accidentally deny ordinary reads.
- Shared workspace, scratch and write roots grant the AppContainer and write
  restrictor in one journaled ACL change per existing object.
- Workspace grants cover directory listing, traversal, metadata and file reads.
  Ancestor compatibility handles expose metadata only, including for paths
  with a trailing separator; they do not enumerate a parent. A workspace that
  contains the trusted recovery journal excludes that protected subtree.
- Existing hardlinks are enumerated across every workspace, write and read
  root before any ACL grant; aliases outside those roots cause refusal without
  a partial grant.
  Preflight keeps NTFS identities and relative paths in a compact plan instead
  of retaining a handle for every file. The grant pass reopens each target,
  rechecks identity and hardlinks, and retains at most 32 target handles while
  its ACL batch is journaled and applied. This bounds handle use and journal
  flushes while retaining one exact rollback intent per object. Read-only roots
  grant only the AppContainer SID; workspace and write roots also grant the
  write-restrictor SID.
- Sensitive trees have package-authority allow ACEs removed and inheritance
  sealed, preserving ordinary host ACEs. These changes are now journaled
  and restored exactly during normal or stale recovery. Per-package deny ACEs alone were insufficient against
  All Application Packages read grants, including on protected child DACLs.
  This behavior needs a production compatibility/security review.
- A private desktop on the launcher's current window station permits USER32/GDI
  startup without granting a broad restricting SID or creating a private
  window station. The station name is queried rather than assumed to be WinSta0.
- A small Detours compatibility DLL forwards NUL/KsecDD device handles,
  requests scoped read/socket capabilities, and injects support into descendants. These hooks are not
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
- The explicit network capability enables a per-call socket broker for ordinary
  IPv4/IPv6 TCP and UDP, including loopback clients and listeners. The broker
  authenticates the exact job/package and transfers sockets only to those
  processes; it does not edit loopback exemptions or firewall configuration.
  `network.ps1` qualifies payload exchange, denials, descendants and teardown.
- Native helpers build from the Windows Cargo build script with MSVC and are
  embedded in the production Windows runtime module. No unsafe Rust was introduced.
- No account provisioning, firewall changes, WSL, or unrestricted retry.

## Native bugs fixed

Outside-write escape through pre-existing workspace hardlinks; credential reads
through protected child DACLs and package allow grants; accidental Git read
denial from generic-write deny masks; a create/assign Job Object race; Cargo
child standard-handle inheritance; Cargo jobserver access to its own isolated
object namespace; linker access to private AppContainer temp storage; cmd.exe
extended/mixed path handling; compatibility handle cleanup on failed startup;
and stale .git reservation cleanup on runner death.

Git inspection and extension hosts now use the fixed-program execution entry
point. Linux still uses mandatory Bubblewrap and explicitly execs that fixed
program. Production Windows entry points now use the same embedded runner.

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
- Historical loopback probes timed out both with and without network
  capabilities. No exemption is configured. Localhost development-server
  connectivity remains unavailable to sandboxed tools.
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

## Historical cleanup follow-up (39111c8)

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
its job handle still requests process termination. The durable recovery journal
described below was added after this historical cleanup-only change.

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
and profiles behind. This paragraph records the historical `39111c8` state.

## Reproduce focused checks

Use native PowerShell as an ordinary user. The outer Codex restricted tool
token cannot stand in for the host user when testing this nested boundary.
No elevation to administrator is required.

Prerequisites: x64 MSVC Rust, Visual Studio C++ Build Tools and Windows SDK,
CMake 3.25+, Git for Windows. Run CMake from an x64 developer command prompt:

```text
cmake -S native/windows/boundary -B target/windows-boundary -G "NMake Makefiles" -DCMAKE_BUILD_TYPE=Release -DLATCH_RECOVERY_TESTING=ON
cmake --build target/windows-boundary
cargo test -p latch-kernel --lib native_shell_and_fixed_git_use_embedded_boundary --locked -- --nocapture
cargo clippy -p latch-kernel --all-targets --locked -- -D warnings
```

Run `test.ps1`, `read_broker.ps1`, `write_broker.ps1`, `adversarial.ps1`,
`lifecycle.ps1`, `network.ps1`, and `scale.ps1` in this directory,
each with `-Binaries <absolute build/bin>` and
`-FixtureRoot <new absolute disposable directory>`.
`scale.ps1` defaults to 4,096 existing files, creates host files while
temporary grants are active, and mutates another batch inside the AppContainer.
Run `developer.ps1 -Mediated` and `scale.ps1 -Mediated` for the production
file broker; the default native backend preserves legacy ACL recovery coverage.
Set `LATCH_BOUNDARY_TIMING=1` to report preflight scan, grant walk, durable
journal, ACL application, and rollback durations separately. The manual Full
validation workflow includes the new network and scale fixtures. They do not
qualify a whole home directory or a reusable grant lease. The scripts refuse
an existing fixture directory. Fixtures contain synthetic data only; do not
point them at actual credential or state directories.

## Loopback policy

Windows' AppContainer loopback policy remains unchanged. With the explicit
Network capability, `socket_broker.{h,cpp}` creates ordinary TCP/UDP sockets
and uses [WSADuplicateSocketW](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsaduplicatesocketw)
to transfer them to an authenticated process in this call's exact Job Object
and AppContainer. The recipient constructs its own descriptor and acknowledges
receipt; the host closes its descriptor immediately. Pending transfers are
bounded to 64, dead recipients are reaped, and all remaining host descriptors
close during call teardown. Raw sockets, other families/protocols and privileged
flags are refused. No machine-wide exemption, firewall rule, account or
unsandboxed child is created. Network permission remains a broad command-level
capability, rather than an endpoint/port grant.

`network.ps1` verifies IPv4 and available IPv6 TCP client/listener payloads,
UDP payloads through ANSI/Wide Winsock entry points, child-shell capability
propagation, denial without Network, forged callers and raw requests, and
listener closure/ACL recovery after launcher cancellation. External DNS uses a
host baseline; private-LAN qualification still requires an explicitly supplied
reachable remote endpoint. Proxies and other IPC protocols remain unqualified.

## Recovery/refactor follow-up (2026-09-27)

See [RECOVERY.md](RECOVERY.md) for the implemented durable protocol, module
ownership, focused failure tests and explicit remaining blockers. The previous
cleanup-only limitations above describe 39111c8, not the new journal.

The AppContainer creation gap now has a pre-journaled unique name and derived
SID. Recovery verifies the profile mapping before cleanup, including crashes
on both sides of the creation API. Deleted grant roots are also recovered by
recorded NTFS identity. See `RECOVERY.md` for cases that still retain a journal.

## Qualification and remaining limits

- **CI:** GitHub Actions run 36388691811 passed the strict native build, all
  six native fixture suites, the focused Windows Rust fmt/clippy/runtime/release
  gate, and the unchanged full Linux Bubblewrap release gate. The complete
  workspace Rust suite also passed locally on Windows with serial tests.
- **Production dogfood:** the real release `latch.exe` completed an NTFS coding
  task with inspection, search, edit, Git, validation, and managed process
  start/poll/terminate followed by revalidation. `latch doctor` and an isolated
  `cargo install --path crates/latch-cli --locked` smoke test passed.
- **Developer tools:** the native fixture ran Git init/commit, a nested Git
  repository, a linked worktree, ripgrep, a `cmd.exe` pipeline, Node/npm,
  Python, and a dependency-free Cargo project inside the boundary.
- **Scope:** Default CI checks a real embedded Windows boundary command and
  CLI startup; the manually triggered `Full validation` workflow runs the six
  native fixture suites and focused Windows Rust tests alongside the complete
  Linux release gate. The dogfood used a deterministic mock provider, so
  live-model behavior is not claimed.
- **Startup:** profile creation and scoped ACL grants cost time on a large
  Windows installation. The runner does not cache grants across commands.
- **Host ACLs:** production source access needs ordinary host read/modify rights,
  without WRITE_DAC. Small runtime/bootstrap and scratch grants remain journaled.
  Package-readable sensitive paths fail closed. See `CURRENT_STATE.md` for
  measured production-broker results and historical ACL-backend limitations.
- **Integration:** CLI host-side Git inspection is read-only preflight;
  command execution itself enters the boundary.
- **Filesystem adversaries:** concurrent grants and revocations beyond the journal lock,
  remaining recovery intervals described in RECOVERY.md, hostile rename/reparse/hardlink races, all existing
  open-handle races beyond the tested Git worktree/common-dir path,
  alternate streams, Unicode/long paths, ACL size limits, network filesystems,
  and conflicts with unrelated host programs.
- **Credentials:** Windows Credential Manager/DPAPI and IPC routes; runtime
  protection against hostile peers; future protected files with explicit
  package grants; complete credential-location coverage and real-home impact
  of persistent ACL hardening. NULL/unsupported sensitive DACLs fail closed.
- **Processes:** Latch managed-process durability and cancellation paths;
  forced runner death at every startup phase; all handle inheritance modes;
  deeper x86 descendants after the direct child probe; descendants bypassing
  compatibility injection; resource
  exhaustion; recovery after cleanup-owner termination or machine crashes.
  Public-launcher termination cleanup is covered by the follow-up tests.
- **Network:** Remote private LAN, proxies, named pipes and other IPC.
  Loopback TCP/UDP IPv4/IPv6 payloads and listener teardown are qualified on
  the tested host; this does not certify every network service/provider.
- **Compatibility:** Python and Windows PowerShell 5.1 extension round-trip
  tests are in manual Full validation but remain unrun; full repository Cargo
  builds under the boundary, VS discovery without an
  explicit developer environment, non-English/Unicode runtime installation
  paths, and KsecDD handle exposure review.
- **Release:** a local ZIP package and installation smoke test exist. Version
  remains 0.2.3; no tag or public release has been made.

The kernel integration module is compiled for production Windows.
