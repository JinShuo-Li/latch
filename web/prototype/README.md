# Latch Web interface prototype

An English, conversation-first interface with a dark default theme for review
before kernel integration. Workspace paths use the interface's sans-serif font.
This is a standalone prototype, not an implementation of `latch --web`.
All conversations, approval requests, tool output, diffs, and agent activity
are illustrative fixtures. Sending messages never contacts a model, executes
commands, changes configuration, or edits workspace files. Preview state lasts
only for the current page; refreshing resets it.

From the workspace you want shown in the interface:

```sh
python3 /path/to/latch/web/prototype/serve.py --port 6006
```

For this repository, from its root:

```sh
python3 web/prototype/serve.py --port 6006
```

Open <http://localhost:6006>. Python 3 is the only preview prerequisite.
The server listens on loopback and serves an explicit asset allowlist, not
the workspace. The displayed workspace is fixed to its launch directory.
Stop the preview server with Ctrl+C.

Review flows:

- Start on the welcome screen; suggestion cards fill the composer.
- Open **Explore an example conversation** for tool details and an example diff.
- Use the top-right button for task details, changes, and example agent activity.
- Send a message to try the simulated working, additional-instruction,
  stop, and allow/deny flows. Nothing executes, including after approval.
- Switch conversations, search titles, and start a new conversation.
- Use the model picker, Ask/Plan/Work selector, and reasoning-effort control.
- Attach or drop images for local previews; image bytes are not uploaded.
- Open Settings for light/dark appearance and example provider/safety controls.
- Try a narrow viewport and the keyboard shortcuts listed in Settings.

The intended connected application will reuse the Rust kernel and existing
session, provider, sandbox, and approval semantics. Computer use remains a
future plugin; this prototype makes no changes to kernel or extension APIs.
