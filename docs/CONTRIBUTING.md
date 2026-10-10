# Contributing

Use a focused checkout and read the [repository instructions](../AGENTS.md)
before changing Latch. Keep fixes scoped, preserve existing workspace changes,
and describe the behavior and validation in your pull request.

## Build and run

Install stable Rust and a C toolchain, plus the platform
[runtime prerequisites](INSTALL.md#runtime-prerequisites). Windows source builds
also require x64 MSVC C++ Build Tools and the Windows SDK.

```sh
git clone https://github.com/JinShuo-Li/latch.git
cd latch
cargo build --locked
cargo run -p latch-cli --locked -- --help
```

## Validation

For significant Rust changes, run the deterministic Linux release gate:

```sh
cargo fmt --all -- --check
bash scripts/release-gate.sh
```

The gate requires a working Bubblewrap sandbox, ripgrep, and Python. Sandbox
probe failures are failures, not skipped coverage. It includes Clippy,
architectural invariants, the workspace suite, and a release build. See
[architecture and CI](ARCHITECTURE.md#testing-and-ci) for platform-specific checks.
Windows runtime suites run natively and serially because ACL recovery uses a
per-user lock.

Paid-provider acceptance, benchmarks, and long-session stress are separate
opt-in checks. Do not add them to default CI. Commands and fixture requirements
are maintained in [AGENTS.md](../AGENTS.md).

## Change boundaries

Keep provider serialization in `provider.rs`, durable truth in the kernel,
and rendering in the interfaces. First-party Rust crates forbid unsafe code.
The `.references/` checkouts are read-only research material; never import,
vendor, or depend on them. Update the affected architecture, continuity, and
runtime documentation when semantics or ownership change.

## Website

The [site maintenance guide](../site/README.md) covers the static documentation
build, local preview, link checks, screenshots, and browser verification.
Repository Markdown remains the content source. Changes to published sources
trigger the GitHub Pages workflow.

## Reporting and review

[Open an issue](https://github.com/JinShuo-Li/latch/issues) with reproduction steps,
platform, Latch version, and relevant diagnostics. Remove credentials and private
workspace data from logs. For changes, use atomic Conventional Commits and explain
what was checked and any remaining limitations.
