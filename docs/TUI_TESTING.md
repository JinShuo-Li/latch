# Real TUI startup and interaction tests

Linux and Apple Silicon macOS smoke CI launch the real `latch` binary inside a
controlling pseudo-terminal (PTY). `scripts/test_tui_pty.py` connects it to an
isolated loopback SSE mock provider and a disposable workspace/config/state.
It does not call a paid provider or use a real credential.

The test reads and parses the actual terminal escape stream with pyte. It
checks the reconstructed screen for startup, composer text and backspace,
two rendered replies, PLAN mode, and a redraw from 120×36 to 92×30. A real
sandboxed `cat proof.txt` must return its fixture contents to the mock provider.
The input prompts must reach the provider after editing. `/quit` must exit 0,
restore the original terminal attributes, and disable the alternate screen,
bracketed paste and mouse reporting.

Run after `cargo build -p latch-cli --locked` (Python 3.11+ and the host's normal
Latch runtime prerequisites are required):

```sh
python3 -m venv /tmp/latch-tui-venv
/tmp/latch-tui-venv/bin/python -m pip install -r scripts/requirements-tui.txt
/tmp/latch-tui-venv/bin/python scripts/test_tui_pty.py target/debug/latch
```

The dependencies are test-only. They do not affect Latch builds or installs.
Debian/Ubuntu may need `python3-venv` to create the test environment.
The harness uses a fresh Python supervisor to own the controlling terminal
while Latch runs, avoiding a Python fork hook in the threaded mock server. It
checks terminal restoration before the supervisor exits; macOS invalidates the
slave terminal when its controlling session leader exits.
Every wait is bounded and failures kill/reap the TUI process group.

GitHub uploads `tui-pty-Linux` and `tui-pty-macOS` artifacts on success or failure:

- `01-startup` through `06-resized`: `.txt` screen grids and `.html` captures
  reconstructed from terminal cells, including their foreground/background colors.
  Download and open the HTML files in a browser to inspect what was rendered.
- `terminal.bin`: actual PTY output bytes, including lifecycle control sequences.
- `mock-requests.json`: isolated mock request bodies, with no HTTP auth headers.
- `report.json`: pass/fail, platform, architecture and checks.
- `terminal-status.json`: real TUI exit code and the supervisor's terminal restoration check.
- `failure.txt` / `failure.html`: the last screen when an assertion fails.

These are real process captures, distinct from Ratatui's deterministic snapshot
fixtures and the website's fixture image. They verify terminal cells and input
behavior, not a specific terminal emulator's font rendering. The test currently
covers Unix PTYs; Windows ConPTY and busy-turn cancellation are not covered.
