# Latch

Latch is a terminal coding agent for Linux and Windows. It keeps sessions on disk and runs commands inside a mandatory platform sandbox. The current version is 0.2.3.

[Website](https://jinshuo-li.github.io/latch/) · [Example configuration](config.example.toml) · [Report a problem](https://github.com/JinShuo-Li/latch/issues)

## Install

Install stable Rust first. Build from this repository:

```sh
cargo install --path crates/latch-cli --locked
```

Linux also needs `bwrap` (Bubblewrap), `rg` (ripgrep), and Git on `PATH`. Windows needs MSVC C++ Build Tools, the Windows SDK, Git for Windows, ripgrep, and an NTFS workspace. The Windows sandbox runner is built into `latch.exe`; WSL and Git Bash are not required.

Check the installation and configuration:

```sh
latch doctor
```

`doctor` reports missing prerequisites without contacting a model provider. Latch refuses to run commands if its sandbox is unavailable.

## Get started

Open a terminal in your project and run:

```sh
latch
```

On first launch, use `/setup` to choose a provider, enter or reference its API key, choose a model, and save. `/model` changes the model for the current session. You can also run one prompt without the TUI:

```sh
latch -p "Explain this repository"
```

Resume a session with `latch --resume`. For scripts, use `latch run --workspace ./project --prompt "Fix the failing test" --output json`; `latch sessions list` shows saved sessions. Run `latch --help` for all options.
Latch asks the model to include its final summary when it marks a task complete;
the recorded validation determines whether that task is verified.
Linux validation preserves failures through output-filtering pipelines. On
Windows, run validation checks separately or redirect output instead of piping it.

## Working safely

`/mode` selects Ask, Plan, or Work. `/safety` controls which operations need approval; `/permissions` controls how approval requests are handled. Every command still runs inside the platform sandbox. Commands classified as inspection run with read-only filesystem access even in Work mode. `/help` lists TUI controls and commands.

The transcript is a compact work log. Ctrl+B opens or closes the session inspector. Mouse dragging selects text through your terminal by default; set `LATCH_MOUSE_CAPTURE=1` before starting Latch to route the mouse wheel to the composer and transcript instead. PageUp/PageDown and Shift+PageUp/Down remain available for scrolling.

On Windows, use a focused checkout owned by your account. Existing files whose ACLs you cannot edit may prevent temporary sandbox access even when you can read them. Very large or actively changing workspaces, especially your whole home directory, can start slowly or fail closed. For a local clone, `git clone --no-hardlinks` avoids links to files outside the workspace. See the [Windows boundary status](native/windows/boundary/CURRENT_STATE.md) and [open issues](https://github.com/JinShuo-Li/latch/issues) for current limits.

## For contributors

Run `cargo fmt --all -- --check` and the platform tests before sending changes. The [architecture](docs/ARCHITECTURE.md), [runtime capability model](docs/RUNTIME_CAPABILITY_MODEL.md), [continuity design](docs/CONTINUITY.md), and [repository instructions](AGENTS.md) contain implementation details. Linux release checks are in `scripts/release-gate.sh`; Windows native fixtures and recovery notes are in `native/windows/boundary/`.
The [Linux benchmark candidate suite](benchmark/README.md) runs isolated CLI coding tasks
against DeepSeek v4.1 Flash and records independent checks, tokens, cache reads,
and wall time. It is opt-in and uses a real provider credential.

Latch is independent software and has no runtime dependency on the upstream projects studied in `.references/`.
