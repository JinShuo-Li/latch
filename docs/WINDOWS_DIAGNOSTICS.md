# Windows findings (2026-10-09)

Earlier measurements in this investigation ran from Ubuntu 24.04 under WSL2 on a Windows host, using
Windows PowerShell 5.1 and native `x86_64-pc-windows-msvc` Rust. Builds and mock
provider tests used a separate, current-user-owned NTFS checkout under `%TEMP%`.
The existing Windows checkout and real Latch configuration were not changed.
No live model calls, ownership changes, firewall changes or recovery-journal
removals were made. The sections below distinguish implemented fixes from remaining qualification gaps.

## Findings and causes

| Symptom | Cause and evidence | Status |
| --- | --- | --- |
| Installed `latch.exe` rejects `--web` | Published v0.2.3 predates Web integration; current source also previously compiled the adapter and its dependencies only on Linux. | Source now enables the same adapter on Windows, with the native Windows URL handler. Updating source does not replace an old installed executable. |
| Successful `dir /b` appears to return no file names | The shared transcript sanitizer treated every carriage return as a progress-line replacement. Windows CRLF therefore erased each completed line. The native boundary returned names correctly; the Web projection contained only `exit 0`. | Fixed in the shared renderer for both TUI and Web. CRLF is a newline; lone CR still replaces progress output. Real-binary Web enumeration regression passes. |
| Enumeration is denied in the existing checkout | Read-only workspace opens now use a per-call host broker, authenticated against the exact Job Object and AppContainer SID. It returns only scoped read handles and never changes source ACLs. Writable commands and sensitive objects already readable by packages still require recoverable ACL changes. | The native `read_broker.ps1` fixture removes WRITE_DAC, verifies real shell/Git/CRT reads, rejects protected paths, outside hardlinks and forged caller/write requests, and compares source ACLs exactly. This fixes the read-only case in [issue #5](https://github.com/JinShuo-Li/latch/issues/5); it does not establish writable foreign-owned-tree support. |
| Large workspaces appear frozen before even a simple command | Each call creates a fresh boundary, scans every workspace/write/read grant root, journals temporary grants, then rolls them back. Command simplicity does not remove workspace preparation cost. | Preflight stores compact identities and relative paths, avoiding one retained handle per object. Grants reopen/revalidate in batches of at most 32, and each checksummed journal record carries up to 32 exact ACL intents. Rollback now preflights and restores identities one at a time, retaining only changed directory handles needed to clean inherited grants from new descendants. Hosted Windows Full validation [37749704283](https://github.com/JinShuo-Li/latch/actions/runs/37749704283) passed the 4,096-file mutable-workspace fixture: 4,129 initial objects, 512 concurrent host files, 256 sandbox mutations, with exact ACL restoration and journal removal. Measured preflight/grant/journal/apply/rollback phases were 185 ms / 6.95 s / 18.73 s / 370 ms / 18.72 s. [Issue #4](https://github.com/JinShuo-Li/latch/issues/4) remains open: ACL mutation and rollback are still per object, and there is no reusable grant lease or whole-home qualification. |
| A changing workspace can block later calls after interruption | Recovery checks object identities, current ACLs and profile ownership before rollback. If concurrent changes prevent proving safe cleanup, it retains the journal and refuses subsequent execution. This is recovery uncertainty, not a model refusal. | Documented in [CURRENT_STATE](../native/windows/boundary/CURRENT_STATE.md) and [RECOVERY](../native/windows/boundary/RECOVERY.md). Read-only workspace opens now avoid the recursive grant pass. Writable-tree scaling and recovery under concurrent mutation still need separate qualification. |
| Sandboxed tools cannot contact a local dev server | AppContainer loopback isolation is separate from internet/private-network SIDs. Explicit Network authorization now enables scoped socket transfers into the authenticated sandbox job/package. | Real Windows native tests pass IPv4/IPv6 TCP client/listener payloads, UDP through ANSI/Wide APIs, child-shell propagation, no-Network/raw/forged-caller denials, and listener closure after launcher cancellation. This implements the local-service use case in [issue #6](https://github.com/JinShuo-Li/latch/issues/6) without firewall rules or machine-wide loopback exemptions. Remote private LAN and proxies remain separately unqualified. |
| Some native edge cases remain unsupported/unqualified | Crash recovery, aliases/reparse races, unsupported DACLs and process compatibility require identity and ownership proofs beyond ordinary command success. These are distinct qualification gaps, not one confirmed common bug. | Hosted Windows Full validation [37750837949](https://github.com/JinShuo-Li/latch/actions/runs/37750837949) passed recovery/adversarial/lifecycle/network/scale/developer/deleted-root native suites. The x64 runner explicitly refuses direct x86 descendants with `ERROR_NOT_SUPPORTED`; deeper x86 descendants remain unqualified. Python extension round-trip passed. The PowerShell 7 failure was reproduced locally: default STA waits throw `FileNotFoundException` in AppContainer. Staging the native PowerShell runtime and using MTA fixes initialization; the real Windows Rust extension round-trip now passes. Bounded structural protocol diagnostics remain for malformed frames. [Issue #7](https://github.com/JinShuo-Li/latch/issues/7) remains open for physical power-loss recovery and hostile-race qualification. |
| Native headless TUI test expects missing mouse ANSI bytes | Crossterm's Windows `EnableMouseCapture`/`DisableMouseCapture` use Win32 console APIs and explicitly report ANSI unsupported. The existing test asserts ANSI bytes were appended to a `Vec`, which is a Unix assumption. | Reproduced as `terminal_screen_commands_toggle_bracketed_paste_symmetrically`. It is a test portability failure; it does not demonstrate broken mouse capture in a real Windows terminal. Left unchanged in this diagnosis-only scope. |

## Small workspace timing probe

Two sandboxed `cmd.exe /d /c dir /b` calls were made for each fresh 10-, 100-
and 1,000-file NTFS fixture, with the required read-only runtime grant. All
calls exited 0, listed every file and restored the root directory's exact SDDL.
These are end-to-end elapsed times, not phase profiles. Only the root SDDL was
compared; these measurements do not certify every object's restoration.

| Files | First call | Second call |
| ---: | ---: | ---: |
| 10 | 219 ms | 302 ms |
| 100 | 592 ms | 661 ms |
| 1,000 | 6,875 ms | 8,335 ms |

No useful warm-call improvement appeared in these fixtures. Disk/antivirus
load and overlapping host activity affect these samples; do not extrapolate
linearly to the previous 625,000-file home probe. The evidence supports
profiling preparation and cleanup before redesigning the boundary.

## Qualification completed for this change

- Native Windows Web tests: authenticated startup, embedded assets, forwarded
  origins, command replay, settings/first setup, resume, attachments, approval,
  cancellation, steering, enumeration, and reasoning activity without exposing
  reasoning text. No external provider or GUI browser is required by this suite.
- Native embedded-boundary shell/Git regression passed, including recursive
  directory enumeration and denied reads outside the workspace.
- Shared activity/transcript tests and native TUI render tests passed, excluding
  the existing Win32/ANSI expectation test described above. An interactive
  Windows terminal input/mouse session was not qualified in this run.
- Linux full release gate, continuity stress, and actual Chromium Web checks
  cover waiting/completion, reconnect, settings, escaped output and mobile layout.

The activity panel reduces uncertainty by showing the real request/tool phase
and time since activity. It cannot distinguish a hung remote model from a slow
model when the provider sends no signal. It reports waiting and silence instead
of guessing that silence means thinking, failure, or completion.
