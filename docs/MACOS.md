# Apple Silicon macOS

The development tree supports native ARM64 macOS 14+ through Seatbelt. Existing
v0.3.2 releases have no macOS archive. Build from source until a new binary
release is published:

```sh
xcode-select --install
brew install ripgrep
cargo install --path crates/latch-cli --locked
latch doctor
latch
# Alternatively: latch --web
```

Install stable Rust first. Git is supplied by the Command Line Tools. Native
builds use `aarch64-apple-darwin`; Intel Macs and Rosetta installations are not
qualified. The Unix installer chooses ARM64 on Darwin, verifies SHA256 using
`sha256sum` or the system `shasum`, and installs to `~/.local/bin`.

The execution boundary uses `/usr/bin/sandbox-exec` and refuses to run if its
startup probe fails. Workspace writes, Git metadata writes, external roots and
IP networking follow existing capability approval. Commands inherit a minimal
environment and writable scratch; credential paths, Keychains and Latch state
are denied. Bash startup profiles are disabled. Child processes inherit
Seatbelt, and cancellation terminates the process group.

GitHub's `macos-15` ARM64 runner compiles and runs the native boundary tests,
then starts the real machine CLI and Web host using isolated state and a local
SSE mock provider. This exercises authentication, HTTP/SSE, command execution
and session operations without a paid provider or real credentials. Local Linux
checks cannot establish macOS runtime success; inspect that CI job's result.

Seatbelt is path policy, not Linux namespaces or a Windows Job Object. Commands
can observe ordinary host files outside the workspace except protected paths.
Private scratch is per backend and cleaned when it is dropped, rather than a
per-command tmpfs. Hardlink aliases, hostile concurrent filesystem changes,
whole-home workspaces and deliberate process-group escape are not qualified;
use a focused checkout without external hardlinks. Interactive TUI rendering,
Gatekeeper distribution, signing and notarization still need Mac qualification.
The workflow ad-hoc signs the stripped binary for ARM64 execution. It has no
Developer ID signature or notarization and does not disable Gatekeeper.

CI qualifies macOS 15 runtime. The deployment target is macOS 14; runtime on
macOS 14 still needs a separate native host because that hosted runner is retired.

Native smoke CI also launches the actual TUI in a controlling PTY, types and
edits prompts, checks replies and a sandboxed tool result, switches mode,
resizes the terminal, and verifies normal exit and terminal restoration.
The `tui-pty-macOS` artifact contains actual screen captures; see
[TUI testing](TUI_TESTING.md).
