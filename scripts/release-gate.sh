#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v bwrap >/dev/null 2>&1; then
  echo 'release gate: bwrap is required for sandbox coverage' >&2
  exit 1
fi
if ! command -v rg >/dev/null 2>&1; then
  echo 'release gate: rg is required for search coverage' >&2
  exit 1
fi

# Match the namespace and mount features used by Latch's own sandbox probe.
if ! bwrap --die-with-parent --new-session --unshare-user --unshare-pid \
    --unshare-ipc --unshare-uts --unshare-cgroup-try --unshare-net \
    --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /run \
    --ro-bind "$PWD" "$PWD" --chdir "$PWD" -- /bin/true; then
  echo 'release gate: Bubblewrap cannot provide the required sandbox namespaces and mounts' >&2
  exit 1
fi

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test -p latch-kernel --test invariants --locked
cargo test --workspace --locked
cargo build --release --locked
