# Private desktop compatibility experiment (2026-09-26)

The prototype retains `WRITE_RESTRICTED`, a single per-call unique restricting
SID, privilege filtering, and administrator deny-only behavior. No broad
restricting SIDs or Low Integrity were added.

`LATCH_PRIVATE_DESKTOP_PROBE=1` enables disposable per-call WindowStation/Desktop
creation in the trusted runner. The unique SID gets station enumeration and
attribute-read rights, plus desktop read/write-object and create-window rights.
The runner retains its token's default object DACL. The child receives an explicit
`STARTUPINFO.lpDesktop`; the guard propagates it to its child. Existing interactive
objects, including `winsta0\default`, are not modified.

## Observed result

On this host, `CreateWindowStationW` returns Win32 error 5 (access denied) in the
trusted runner. Reducing the requested handle rights does not change the result.
A native USER32/GDI control executable launched directly, without the Latch
restricted token, reaches `wmain` but also gets error 5 creating a private station
with default Windows security. Thus the private-desktop experiment is blocked
before the restricted child starts. Guard/hook/Bash startup and outside-write
denial under a private desktop remain unverified.

This is not evidence that Bash failed under a successfully created private
desktop. The earlier loader trace identifies USER32 process attach failing when
GDI initialization returns `STATUS_UNSUCCESSFUL`; `NtGdiInit2` returns zero.
Deeper undocumented GDI investigation is stopped.

## Product alternatives

- Broad keep-alive restricting SIDs provide partial write confinement and leave
  the known outside-write escape; this does not satisfy the required boundary
  and must not be merged as a secure backend.
- Change the model-facing Windows shell/runtime and validate its real subprocess
  capability matrix with the unique-only write-restricted token.
- Require a stronger identity or container boundary and validate that boundary.

No merge, Windows CI, or dogfood is authorized by these results. This diagnostic
prototype is retained only on the experimental branch. Reproduce the private-station attempt with
`cargo test -p latch-kernel --lib restricted_loader_probe_ladder -- --ignored --nocapture`.
For the direct control, set `LATCH_PROBE_STATION=1` and run the built
`loader-probe-user32.exe` from the native build output.
