# Docker dogfood harness

Run Latch against a disposable workspace inside a container, inspect the
result, and throw the environment away. The harness drives the existing
machine CLI (`latch run` / `latch resume` / `latch sessions`); it does not add a
second execution path, a kernel mode, or an alternative provider stack.

## Purpose

Two related jobs:

- **Manual dogfooding** on any Linux host, including a Raspberry Pi 5, without
  installing Bubblewrap, `rg`, or a Rust toolchain system-wide.
- **Deterministic integration coverage** of that environment, so the harness is
  known to work before it is trusted for real-provider runs.

Provider behavior is unchanged. The container only supplies the filesystem,
sandbox, and process boundary.

## Architecture and trust boundary

```
host: fixture/workspace  ─────►  /workspace (rw)   container: latch run …
      host state dir      ─────►  /state     (rw)
      host home dir       ─────►  /home/latch (rw)
      config file         ─────►  /config/config.toml (ro)
                                   │
                                   └─ latch process ─► provider over the network
                                      │
                                      └─ mandatory Bubblewrap sandbox for every
                                         tool/command (unchanged kernel policy)
```

Boundary properties:

- **Non-root.** The image defaults to an unprivileged `latch` user; the runner
  additionally matches the host `uid:gid` so mounted files keep clear ownership.
- **Not privileged.** No `--privileged`, no `--network host`, no Docker socket,
  no host home or credential mounts. Only the paths above are mounted.
- **Explicit writes only.** Containers run with `--rm`; the workspace, state,
  and home are the only host paths that can be written, and they live under the
  run directory unless `--workspace` names a host directory.
- **Bubblewrap still runs.** Latch requires unprivileged user namespaces for
  its sandbox. Docker's default seccomp profile and system-path masking block
  the nested mounts Bubblewrap needs, so the harness passes exactly
  `--security-opt seccomp=unconfined --security-opt systempaths=unconfined`.
  These are not `--privileged` and do not grant extra capabilities. The inner
  Latch sandbox (read-only host root, private `/tmp`, masked `~/.ssh`, GnuPG,
  cloud and registry credentials, no network unless granted) is unchanged.
- **No secrets in the image.** The image contains no API keys, tokens, SSH keys,
  or user configuration. Credentials are forwarded one variable at a time and
  never written to the image, the config, or the durable event log.

## Build

```sh
docker build --target runtime -t latch-dogfood:local -f docker/Dockerfile .
```

`scripts/dogfood.sh` builds this target automatically when the image is
missing. The build is multi-stage; only the stripped `latch` binary and the
runtime packages land in the final image.

Requires a recent Docker Engine with BuildKit (the default since Docker 23) and
the `systempaths` security option (Docker 25+). The `# syntax` directive and the
Cargo cache mounts are BuildKit features; the harness does not support the
legacy builder.

## Deterministic test command

No network, credentials, TTY, or live model:

```sh
./scripts/dogfood-test.sh
```

It builds the `test` image stage (runtime plus the loopback mock provider) and
asserts, inside and outside the container:

- the image builds and the container starts;
- the workspace is visible and mutations are observable on the host;
- `latch run --output json` executes through the real machine interface;
- provider and configuration failures propagate as non-zero exit codes
  (`1` runtime, `2` configuration, `3` unresolved permission);
- isolation holds: non-root, no default route, no Docker socket, no privileged
  capability set, and the Bubblewrap sandbox runs inside the container.

The mock is `crates/latch-cli/examples/mock_provider.rs`, the process-level
counterpart of the in-process provider used by `crates/latch-cli/tests/cli.rs`.
It is started on loopback and served over `--network none`, so no paid or
external API is involved. This test runs outside `cargo test` on purpose; it is
documented here and in `AGENTS.md` rather than wired into CI.

## Manual real-provider run

Configure a provider once, then forward only its credential variable:

```sh
cp docker/config.dogfood.toml /tmp/latch-dogfood.toml
$EDITOR /tmp/latch-dogfood.toml          # set [providers.*] and [inference]

OPENCODE_API_KEY=… ./scripts/dogfood.sh \
  --config /tmp/latch-dogfood.toml \
  --provider-env OPENCODE_API_KEY \
  --fixture ./some/fixture \
  --prompt "Fix the failing test and run it"
```

Useful options:

- `--workspace DIR` mounts an existing host directory read-write instead of
  creating a disposable one; `--fixture DIR` seeds the disposable workspace.
- `--model`, `--provider`, `--mode`, `--output` map to the same-named CLI flags.
- `--network bridge` (default) lets the provider reach the network; use
  `--network none` for offline runs.
- `--shell` opens an interactive shell in the container with the same mounts.

Real credentials are read from the host environment by name and passed with
`-e NAME`; the value is never printed by the harness. Latch already redacts
credentials from config, logs, the transcript, and the event log.

## Workspace mount behavior

- The container workspace is always `/workspace`; with no `--workspace`, the
  runner creates `target/dogfood/<run-id>/workspace` and optionally copies a
  `--fixture` into it.
- The Latch state directory is always `/state` and must match `state_dir` in
  the mounted config (`docker/config.dogfood.toml` uses `/state`).
- The container home is `/home/latch`, backed by the run directory, so tool
  caches and Git identity stay inside the run.
- The config is mounted read-only; `/setup` is not used in machine mode.
- Files created by the agent are owned by the host user because the container
  runs as the host `uid:gid`.

Everything for a run is kept under `target/dogfood/<run-id>/` (workspace,
`state/`, `home/`, `out/`) so the diff can be inspected. `target/` is ignored
by Git.

## Credentials handling

- Never baked into the image or the config template.
- Forwarded one variable at a time with `--provider-env NAME` / `-e NAME`.
- Stored in the run directory's state only if Latch itself persists a secret
  through `/setup`; machine mode does not.
- Not printed by the harness. Latch keeps credentials out of structured output,
  logs, transcripts, and the durable event log.

## Cleanup

- Containers always run with `--rm`; no dangling containers or volumes.
- The run directory is kept by default for inspection. Delete it with
  `--remove` or `rm -rf target/dogfood/<run-id>`.
- Reclaim Docker build cache with `docker builder prune` if disk is tight.

## ARM64 notes

The Dockerfile uses `rust:1-bookworm` and `debian:bookworm-slim`, both of which
publish `arm64` and `amd64`, so the same file builds on a Raspberry Pi 5 and on
amd64 Linux. No `--platform` flag is needed; the image is native to the host.

- On a 4 GB Pi, cap Cargo parallelism with
  `CARGO_BUILD_JOBS=2 ./scripts/dogfood.sh` (the runner passes it through as a
  build arg). Cargo's own defaults are used otherwise.
- BuildKit cache mounts persist the Cargo registry, Git checkouts, and target
  directory across builds, so iterating on sources does not recompile every
  dependency.
- Bubblewrap runs as a non-root user inside the container; no setuid sandbox is
  required.

## Known limitations

- The runtime image ships `latch`, `bubblewrap`, `ripgrep`, `git`,
  `ca-certificates`, and `python3`. It is not a general development image; a
  task that needs another toolchain (Node, Go, a JDK, …) should use a derived
  image or mount a pre-built toolchain.
- Providers that listen on the host (`--network host` is deliberately not used)
  are reachable only if published through a bridge-reachable address; a
  container-local endpoint works at `127.0.0.1`.
- The deterministic mock is a scripted SSE responder, not a model: it exercises
  the harness and CLI contract, not model quality.
- Unprivileged nested user namespaces require the two `--security-opt` flags
  above; a host that forbids them entirely cannot run the sandboxed image.
- Docker (or a Docker-compatible CLI via `DOCKER=…`) is a prerequisite of this
  harness only; Latch itself has no container dependency.
