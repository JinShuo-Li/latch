# Experimental Windows recovery protocol

Scope: native candidate only, starting from `39111c8`. Production Windows
execution remains disabled; this is not a release or a complete port.

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
starting a command. No Latch production startup integration is enabled.

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

A missing `.git` is created privately as a delete-on-close reservation. Its
file identity and destination parent identity are durable before a handle
rename publishes it. Cross-volume publication fails closed. AppContainer
creation intent records original absence, the nonce name, derived SID,
package path and parent identity; after the API returns, the actual package
identity is sealed before grants or child launch. See the remaining gap below.

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

AppContainer SID derivation, package path, registry moniker, directory identity
and absence of reparses are verified before profile cleanup. Resource handles
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

Recovery assertions cover: after the header; before profile creation; first
ACL; all grants; sealed AppContainer creation; suspended child launch; four
live generations; first normal rollback; rollback seal; profile deletion;
reboot-equivalent normal startup; torn intent. Each successful recovery checks
exact original workspace/runtime/sensitive ACLs, no reservation, no package or
registry profile, no pending journal, and a second safe recovery. Corrupted
records, unrelated ACL edits and replacement objects must fail closed without
overwriting host state. An unsealed profile creation is an explicit refusal
test, not a successful automatic recovery result.

## Remaining recovery blockers

1. **AppContainer creation has an unsealed interval.** Windows'
   CreateAppContainerProfile replaces a precreated package directory, including
   a populated directory held open without delete sharing (observed moved to
   NTFS `$Extend/$Deleted`). Pre-recording a staged file ID therefore does not
   describe the actual resource. A profileless token probe failed process
   creation. If the owner dies inside the API or before the returned package
   ID is durably recorded, startup retains the creation intent and fails
   closed with operator-reconciliation diagnostics. It does not delete an
   unverified directory. This interval is tested and **prevents claiming P0
   fully complete** or enabling production execution.
2. Physical reboot/power-loss has not been performed. The automated test
   removes every relevant process and handle; durability assumes NTFS and
   storage honoring flush/write-through requests.
3. Existing-object deletion/rename during a command may make exact rollback
   impossible; startup refuses rather than recreating names or following
   aliases. There is no operator repair UI yet.
4. Active unrelated ACL mutation, hostile same-user processes, cross-volume
   reservation staging, and all rename/reparse race interleavings need further
   review. The journal deliberately retains conflicts.
5. Large recursive grant roots produce many durable records. Toolchain docs
   should not be included in focused build fixtures; production performance
   and root selection need work. Test-only journal overrides must not run
   overlapping grant roots concurrently; the production root has one lock.

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
- `recovery.ps1`: 16 focused cases passed their expected outcomes: 12 automatic
  rollback/idempotence cases and four deliberate fail-closed cases (unsealed
  API creation, corrupt record, host ACL conflict, replaced object).
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

These gates were run on September 27, 2026. The AppContainer unsealed-creation
case remains a tested refusal and an unresolved P0 requirement. Passing the
other gates does not authorize production integration.

No physical reboot/power-cut, full native Windows workspace gate, Windows CI,
production integration, doctor/version changes or NTFS agent dogfood is claimed.
