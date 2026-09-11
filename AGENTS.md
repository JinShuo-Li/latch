# Latch repository instructions

`.references/pi` is an ignored, shallow, read-only research checkout of Pi. It
exists to study interaction patterns, agent-loop behavior, providers, tools,
sessions, and terminal UI. Never edit, commit, vendor, import, link against, or
add a dependency on anything below `.references/`. Latch must remain an
independent implementation. The reviewed revision is pinned in
`references/pi.rev`.

First-party Rust crates forbid unsafe code. Preserve unrelated and pre-existing
workspace changes, ground mutations against observed file hashes, and never use
destructive Git recovery for undo.
