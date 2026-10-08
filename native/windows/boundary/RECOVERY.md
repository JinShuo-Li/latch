# Windows native recovery protocol

Production Windows execution uses this protocol. Test builds add isolated
crash hooks; Cargo's embedded runner does not include them.

## Protocol and ownership

The public launcher owns a lifetime handle. Its trusted cleanup owner owns
the restricted AppContainer child, private desktop, and kill-on-close Job
Object. The child is placed in the job atomically at creation. Normal exit,
timeout, and cancellation drain every descendant before rollback. Owner
death closes the last job handle; recovery also opens the recorded unique
job name and drains it before touching any ACL. No PID-based ownership guess
is used. A reboot-equivalent test kills both trusted processes and all four
descendant generations, then starts an ordinary new runner.

`Recovery` first locks the user-only recovery directory under the trusted
LocalAppData known folder (`LatchBoundaryRecovery-v1`). One transaction is
active per user. It automatically recovers `pending` before accepting a new
command. `--recover-only` exposes that same trusted startup path without
starting a command. Production launches use the same recovery path first.

Before a host-persistent change, a bounded UTF-16 record is written with a
version, length, sequence and CRC32. Records use CREATE_NEW, WRITE_THROUGH,
FlushFileBuffers, close, then a same-volume write-through rename. Published
records are immutable. Truncated/unpublished temporary records cannot have
authorized a mutation and are discarded. A corrupted published record or
a sequence gap fails closed and remains available for diagnosis. Directory,
transaction, and record ACLs are protected and permit only the current user
and SYSTEM. This is integrity against sandbox children, not an administrator
or a malicious process already running with the trusted user's full token.

ACL intents contain the exact original and intended owner/group/DACL SDDL,
including ACE order and inheritance/protection controls. SACLs are never
modified. Existing descendants are journaled explicitly; applying an ACL
does not propagate unrecorded changes. Sensitive-path sealing is temporary
and is restored with the same mechanism. New developer-created objects
have no pre-call ACL: recovery removes only the transaction's unique SID
ACEs, retaining all unrelated entries, and journals those removals too.
On roots shared by the AppContainer and write-restrictor identities, both
allow ACEs are applied in one ACL mutation and one intent per existing object.
Before child execution, rollback scans only subtrees with a recorded ACL
mutation or possible inheritance from one. After execution starts it scans
every grant root, since a child may rename a granted directory. Unrecorded
objects are opened with read-only metadata
rights first; `WRITE_DAC` is requested only if this transaction's ACE is
present, with identity and descriptor rechecked before writing. An unrelated
inaccessible host object outside affected subtrees is never opened.
When a workspace contains the trusted recovery directory (for example a home
workspace), grant and rollback traversal exclude that directory and its
descendants. Its protected user/SYSTEM DACL remains unchanged; requesting the
journal itself as a grant root is still refused.

A missing `.git` is created privately as a delete-on-close reservation. Its
file identity and destination parent identity are durable before a handle
rename publishes it. Cross-volume publication fails closed. AppContainer
creation intent records original absence, the unique nonce name, derived SID,
package path and parent identity. Both the mapping and directory are rechecked
for absence immediately before publishing the intent. The profile identity is
the name and derived SID with its matching registry moniker, not the NTFS ID
of a directory that CreateAppContainerProfile may replace. The directory ID
is additionally sealed after the API returns.

Rollback validates all recorded host objects first, restores original ACLs,
removes new-object grants and reservations, then deletes the verified profile.
A durable rollback seal permits restarting after partial profile deletion;
external ACLs are still checked after that seal. Only a durable completion
marker authorizes deletion of journal records. An interrupted journal unlink
resumes from that marker. The journal is never discarded on failed rollback.

## Resource validation

Each file identity includes NTFS volume serial, 128-bit file ID, creation time
and directory type. Every path component is opened without delete sharing
and with OPEN_REPARSE_POINT; reparses and nonlocal/non-NTFS roots are refused.
ACL calculation, identity check and mutation share the same target handle.
Recovery accepts only the recorded original/intermediate descriptors. A host
ACL edit or replacement is a conflict: no approximate ACE subtraction is
used to overwrite an existing object's ACL. Diagnostics name the path and
retained journal. The caller must reconcile the host change before retrying.

AppContainer SID derivation, package path, registry moniker and absence of
reparses are verified before profile cleanup. A sealed directory ID is also
verified when available. An unsealed directory without a matching mapping
is ambiguous and retained with the journal, not deleted. Resource handles
are scoped and noncopyable. This is not a claim of atomic compare-and-swap
against a fully privileged host process concurrently changing ACLs.

## Tests and reproducibility

Build the ordinary candidate with CMake and MSVC `/W4 /WX`. For disposable
crash fixtures only, add `-DLATCH_RECOVERY_TESTING=ON` and run:

```powershell
./native/windows/boundary/recovery.ps1 -Binaries <absolute build/bin> -FixtureRoot <new disposable NTFS directory>
```

The option defaults OFF. Cargo's embedded native runner never compiles these
hooks. Test builds accept an isolated journal root and pause marker; the test
forcibly kills the actual cleanup-owner PID, rather than throwing an exception
that could unwind destructors. All credentials are synthetic. Negative cases
retain their journal and fixture for inspection.

Recovery assertions cover: after the header; before profile creation; the
unsealed interval after the API returns; first ACL; all grants; sealed
AppContainer creation; suspended child launch; four
live generations; first normal rollback; rollback seal; profile deletion;
reboot-equivalent normal startup; torn intent. Each successful recovery checks
exact original workspace/runtime/sensitive ACLs, no reservation, no package or
registry profile, no pending journal, and a second safe recovery. Corrupted
records, unrelated ACL edits and replacement objects must fail closed without
overwriting host state. A matching unsealed mapping recovers automatically;
an unregistered directory or a mismatched mapping retains the journal.

## Remaining recovery risks

1. The unsealed API interval now recovers by the matching unique profile
   name/derived SID registration and moniker, after durable absence checks.
   CreateAppContainerProfile can replace a precreated directory, so its ID
   cannot serve as advance authority. A directory without a registration
   remains ambiguous and is retained; a wrong moniker is also refused. The
   tests kill the owner immediately before and after the API, not at an
   arbitrary instruction inside the Windows API itself.
2. Physical reboot/power-loss has not been performed. The automated test
   removes every relevant process and handle; durability assumes NTFS and
   storage honoring flush/write-through requests.
3. Existing-object deletion/rename during a command uses the recorded NTFS ID;
   absent objects are skipped only after an absence check. Replacement, access
   denial, and unrelated ACL changes retain the journal. There is no operator
   repair UI yet.
4. Active unrelated ACL mutation, hostile same-user processes, cross-volume
   reservation staging, and all rename/reparse race interleavings need further
   review. The journal deliberately retains conflicts.
5. Large recursive grant roots still produce one logical ACL intent and
   mutation per existing object. Up to 32 exact intents share one durable
   record, but production performance and whole-home recovery are not yet
   qualified. Toolchain docs should not be included in focused build fixtures.
   Test-only journal overrides must not run overlapping grant roots
   concurrently; the production root has one lock.

## Module ownership

- `runner.cpp`: policy ordering and trusted/public entry points.
- `token.*`: identities and restricted-token default DACL.
- `appcontainer.*`: profile API, registry identity, private object namespace.
- `filesystem_acl.*`: grants, sensitive sealing, hardlink validation, Git marker.
- `object_security.*`: pinned NTFS identity and exact descriptor mechanism.
- `recovery.*`: transaction ownership and intent creation.
- `recovery_store.*`: bounded durable record encoding/decoding.
- `recovery_rollback.cpp`: preflight, rollback order and journal retirement.
- `desktop.*`, `job.*`, `process.*`: native resource lifetimes and process launch.
- `compat.cpp`: existing narrow device/descendant compatibility hooks.

The Rust/native interface remains unchanged and test-only. No native unsafe
code has moved into the Rust kernel. No account, firewall, or system-policy
provisioning is performed. Linux Bubblewrap policy is unchanged.

## Validation ledger (this branch)

- CMake x64 MSVC runner/DLL/fixture: `/W4 /WX` build passed after refactor.
- `cargo fmt --all -- --check`: passed.
- Kernel Clippy, all targets/all features, locked, `-D warnings`: passed.
- `recovery.ps1`: 18 focused cases passed on September 27, 2026: automatic
  rollback/idempotence including owner death before and immediately after the
  profile API; mismatched mapping and unregistered-directory conflicts retain
  the journal, as do corrupt records, host ACL conflicts and replacements.
- `test.ps1`: all 18 baseline assertions and four-generation teardown passed.
- `adversarial.ps1`: all assertions passed, including outside hardlink refusal,
  junction/NULL-DACL writes, protected credentials and Git reservation.
- `lifecycle.ps1`: four-generation timeout/root-exit/launcher-kill, exact
  ACL/profile cleanup and failed-executable cleanup passed.
- Evidence: `C:/project/latch/target/recovery-final-{crash,baseline,adversarial,lifecycle}`.
- A first broad-root Cargo fixture compiled, passed its unit test and an actual
  rustdoc test, but cleanup correctly refused overlapping temporary grants
  from a concurrent integration fixture using a different test journal root.
  It is **not** counted as a successful complete run. The first Rust fixture
  was cancelled during oversized toolchain journaling, also not a pass.
- Identity-checked operator reconciliation of that fixture overlap completed:
  74,969 ACLs restored, one already-deleted disposable fixture object recorded,
  and both profiles removed. Original journals are archived, with evidence
  under `target/recovery-overlap-reconciliation`. The repair is a disposable
  operator tool, not product recovery behavior.
- Fresh sequential `cargo.ps1` passed compile/link, one unit test, one actual
  rustdoc test, and cleanup. Evidence: `target/recovery-final-cargo`. Its
  read roots include toolchain `bin` and `lib`, not the HTML documentation.
- The Rust embedded-runner test compiled with `cargo test -p latch-kernel
  --lib native_shell_and_fixed_git_use_embedded_boundary --locked --no-run`.
  The resulting real Rust test executable then passed that test with isolated
  disposable `RUSTUP_HOME`/`CARGO_HOME`, and `GOPATH`/`JAVA_HOME`/`VIRTUAL_ENV`
  unset. Result: one passed, zero failed, 375 filtered out, 0.91 seconds; no
  pending default journal remained. Evidence: `target/recovery-embedded-final`.
  This tests native shell reads, denied writes and fixed Git execution through
  the embedded runner; it is not the full Windows Rust workspace suite.
- Linux GitHub Actions CI passed for implementation checkpoint
  `f8421c67ce517eba4ffa0c56f71997b983096612`: run `36298360007`,
  <https://github.com/JinShuo-Li/latch/actions/runs/36298360007>. The unchanged
  Ubuntu reliability job runs the mandatory Bubblewrap security probe plus
  fmt, workspace Clippy, invariants, full workspace tests and release build.

Sequential retry commands (native fixture builds need the MSVC x64 developer
environment; use a new disposable directory for each run):

```powershell
./native/windows/boundary/cargo.ps1 -Binaries <absolute build/bin> -FixtureRoot <new disposable NTFS directory> -Toolchain <absolute rustup toolchain>
cargo test -p latch-kernel --lib native_shell_and_fixed_git_use_embedded_boundary --locked --no-run
# Run the test executable reported by --no-run in a fresh process with:
# RUSTUP_HOME and CARGO_HOME = an empty disposable directory
# GOPATH, JAVA_HOME and VIRTUAL_ENV unset
& <reported latch_kernel test executable> native_shell_and_fixed_git_use_embedded_boundary --nocapture
```

The mapping-based recovery revision passed the `/W4 /WX` native build and
`recovery.ps1`, `test.ps1`, `adversarial.ps1`, and `lifecycle.ps1` on September
27, 2026 (fixtures `target/recovery-mapping-04` and `target/mapping-*`).
This closes the tested unsealed-creation refusal, but does not itself certify
the wider production boundary.

## Additional Windows qualification fixtures (2026-10-08)

The current branch adds `network.ps1` for loopback client and listening-server
policy checks, and `scale.ps1` for a large mutable workspace with concurrent
host-created files and sandbox-created/renamed/deleted files. The scale fixture
sets `LATCH_BOUNDARY_TIMING=1` to report preflight, grant walk, journal, ACL
application, and rollback durations. Both scripts are part of the manual Full
validation workflow. Their results are pending a Windows run; they are not
counted as passes in the validation ledger above.

Workspace, write-root and read-root preflight stores compact NTFS identities
and relative paths rather than retaining every file handle. Every root
completes hardlink validation before any ACL grant. The grant pass reopens
objects through checked path components, verifies the saved identity and
rechecks hardlinks; only a bounded batch of up to 32 target handles stays open
at once. Workspace and read-root grants publish up to 32 exact ACL intents in
one checksummed write-through record before applying any ACL in that batch.
After a crash, recovery accepts either the original or granted descriptor for
each row, so a partially applied batch can be rolled back without guessing.
Recovery preflights journaled identities one at a time, then reopens and
rechecks each object's ACL immediately before restoring it. During descendant
cleanup it retains handles only for changed directories that can pass a
temporary inherited ACE to new children, not one open file handle per journal
row. ACL changes and logical recovery intents remain per object; the scale
fixture must qualify the change before performance claims are made.

The loopback policy remains default-deny for sandboxed commands. The network
fixture checks an externally resolvable DNS name, resolves `localhost`, checks
IPv4 and available IPv6 client/listener paths, and can test a caller-supplied
reachable private-LAN endpoint. The
runner does not edit the operating system's AppContainer loopback configuration
or firewall. The lifetime and preservation requirements for Windows' debugging
exemption mechanisms are described in the native boundary README. A scoped
relay remains unimplemented. The lifecycle fixture probes x86 top-level
startup and one direct x86 child on WOW64; deeper descendants and compatibility
injection bypasses remain unqualified. Python and Windows PowerShell 5.1
extension round-trip tests are in the Windows Full validation matrix but have
not run here. Physical power-loss and hostile rename/reparse/hardlink races
also remain open.

No physical reboot/power-cut, full native Windows workspace gate, Windows CI,
production integration, doctor/version changes or NTFS agent dogfood is claimed.
