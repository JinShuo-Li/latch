#!/usr/bin/env bash
#
# Deterministic Docker dogfood integration test.
#
# Builds the `test` image stage and exercises the real machine CLI end-to-end
# against the loopback mock provider with no network, no credentials, and no
# TTY. This is intentionally separate from `cargo test`; run it explicitly:
#
#   ./scripts/dogfood-test.sh
#   ./scripts/dogfood-test.sh --no-build      # reuse an existing test image
#   KEEP=1 ./scripts/dogfood-test.sh          # keep artifacts for inspection
#
# It asserts the harness contract: the image builds, the container starts, the
# workspace is visible, the machine CLI runs, produced files are observable on
# the host, failures propagate as non-zero exit codes, and isolation is intact.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
docker="${DOCKER:-docker}"
image="${LATCH_DOGFOOD_TEST_IMAGE:-latch-dogfood:test}"
build="auto"

while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) build="never"; shift ;;
        --build) build="always"; shift ;;
        --image) image="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,20p' "$0"
            exit 0
            ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

if ! command -v "$docker" >/dev/null 2>&1; then
    echo "error: docker is not installed or not on PATH" >&2
    exit 2
fi

pass=0
fail=0
ok() { echo "ok   - $*"; pass=$((pass + 1)); }
bad() { echo "FAIL - $*" >&2; fail=$((fail + 1)); }

work="$root/target/dogfood-test/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$work"

host_uid="$(id -u)"
host_gid="$(id -g)"
if [ "$host_uid" -eq 0 ]; then
    host_uid=1000
    host_gid=1000
fi

# --- build ---------------------------------------------------------------
case "$build" in
    always) do_build=1 ;;
    never) do_build=0 ;;
    auto)
        if "$docker" image inspect "$image" >/dev/null 2>&1; then
            do_build=0
        else
            do_build=1
        fi
        ;;
esac

if [ "$do_build" -eq 1 ]; then
    echo "building $image (target: test) ..."
    if "$docker" build --target test -t "$image" -f "$root/docker/Dockerfile" "$root"; then
        ok "image build (target: test)"
    else
        bad "image build (target: test)"
        echo "cannot continue without an image" >&2
        exit 1
    fi
else
    ok "reused existing image $image"
fi

# Latch's mandatory Bubblewrap sandbox needs unprivileged user namespaces;
# Docker's default seccomp and system-path masking block the nested mounts.
declare -a base=(
    --rm
    --network none
    --security-opt seccomp=unconfined
    --security-opt systempaths=unconfined
    --user "$host_uid:$host_gid"
    -e HOME=/home/latch
)

# --- container starts and the CLI executes ------------------------------
if version="$("$docker" run "${base[@]}" --entrypoint latch "$image" --version 2>/dev/null)"; then
    if printf '%s' "$version" | grep -q "latch"; then
        ok "container starts; latch --version = $(printf '%s' "$version" | head -1)"
    else
        bad "latch --version output unexpected: $version"
    fi
else
    bad "container failed to start latch --version"
fi

# --- success path: workspace visible, file produced, output observable ---
ws="$work/ws-success"
state="$work/state-success"
home="$work/home-success"
out="$work/out-success"
mkdir -p "$ws" "$state" "$home" "$out"
cat > "$work/mock-success.json" <<'JSON'
[
  {"tool_call": {"name": "write", "arguments": {"path": "dogfood.txt", "content": "hello from latch"}}},
  {"text": "wrote dogfood.txt"}
]
JSON

set +e
"$docker" run "${base[@]}" \
    -v "$ws:/workspace" -v "$state:/state" -v "$home:/home/latch" \
    -v "$work/mock-success.json:/mock/script.json:ro" \
    -e DOGFOOD_SAFETY=standard \
    "$image" > "$out/result.json" 2> "$out/stderr.log"
status=$?
set -e

if [ "$status" -eq 0 ]; then
    ok "machine run succeeds (exit 0)"
else
    bad "machine run exit $status (expected 0); stderr:"; sed 's/^/       /' "$out/stderr.log" >&2
fi

if [ "$(cat "$ws/dogfood.txt" 2>/dev/null || true)" = "hello from latch" ]; then
    ok "workspace visible and mutated; host observes dogfood.txt"
else
    bad "dogfood.txt missing or wrong content on host"
fi

if grep -q '"status":"completed"' "$out/result.json"; then
    ok "structured status is completed"
else
    bad "structured status not completed: $(cat "$out/result.json" 2>/dev/null || true)"
fi

if [ "$(wc -l < "$out/result.json")" -eq 1 ]; then
    ok "stdout is exactly one JSON line"
else
    bad "stdout is not a single JSON line"
fi

if grep -q '"workspace":"/workspace"' "$out/result.json"; then
    ok "result reports the container workspace /workspace"
else
    bad "result does not report /workspace"
fi

# --- failure propagation: unreachable provider is a non-zero runtime fail -
ws_fail="$work/ws-fail"
state_fail="$work/state-fail"
home_fail="$work/home-fail"
out_fail="$work/out-fail"
mkdir -p "$ws_fail" "$state_fail" "$home_fail" "$out_fail"

set +e
"$docker" run "${base[@]}" \
    -v "$ws_fail:/workspace" -v "$state_fail:/state" -v "$home_fail:/home/latch" \
    -e DOGFOOD_MOCK=0 -e DOGFOOD_BASE_URL=http://127.0.0.1:1/v1 \
    "$image" > "$out_fail/result.json" 2> "$out_fail/stderr.log"
status=$?
set -e

if [ "$status" -ne 0 ]; then
    ok "provider failure propagates as non-zero exit ($status)"
else
    bad "provider failure exited 0"
fi
if grep -q '"status":"failed"' "$out_fail/result.json"; then
    ok "failure status is reported as failed"
else
    bad "failure status not reported: $(cat "$out_fail/result.json" 2>/dev/null || true)"
fi

# --- permission model: strict workspace write is denied, never approved ---
ws_perm="$work/ws-perm"
state_perm="$work/state-perm"
home_perm="$work/home-perm"
out_perm="$work/out-perm"
mkdir -p "$ws_perm" "$state_perm" "$home_perm" "$out_perm"

set +e
"$docker" run "${base[@]}" \
    -v "$ws_perm:/workspace" -v "$state_perm:/state" -v "$home_perm:/home/latch" \
    -v "$work/mock-success.json:/mock/script.json:ro" \
    -e DOGFOOD_SAFETY=strict \
    "$image" > "$out_perm/result.json" 2> "$out_perm/stderr.log"
status=$?
set -e

if [ "$status" -eq 3 ]; then
    ok "unresolved permission exits 3 (denied, not auto-approved)"
else
    bad "strict write exit $status (expected 3)"
fi
if [ -e "$ws_perm/dogfood.txt" ]; then
    bad "denied write leaked into the workspace"
else
    ok "denied write did not mutate the workspace"
fi

# --- configuration errors are exit 2 ------------------------------------
set +e
"$docker" run "${base[@]}" --entrypoint latch "$image" \
    run --config /nonexistent.toml --prompt "x" --output json \
    > "$work/config-error.json" 2>/dev/null
status=$?
set -e
if [ "$status" -eq 2 ]; then
    ok "missing config is a non-zero configuration error (exit 2)"
else
    bad "missing config exit $status (expected 2)"
fi

# --- isolation assumptions ----------------------------------------------
uid_out="$("$docker" run "${base[@]}" --entrypoint sh "$image" -c 'id -u' 2>/dev/null || echo error)"
if [ "$uid_out" != "0" ] && [ "$uid_out" != "error" ]; then
    ok "container runs as non-root (uid $uid_out)"
else
    bad "container uid is $uid_out (expected non-root)"
fi

if "$docker" run "${base[@]}" --entrypoint sh "$image" -c 'test ! -e /var/run/docker.sock' >/dev/null 2>&1; then
    ok "docker socket is not exposed inside the container"
else
    bad "docker socket is visible inside the container"
fi

if "$docker" run "${base[@]}" --entrypoint sh "$image" \
    -c 'grep -q "00000000" /proc/net/route && exit 1 || exit 0' >/dev/null 2>&1; then
    ok "no default route inside the container (network = none)"
else
    bad "container unexpectedly has a default route"
fi

if "$docker" run "${base[@]}" --entrypoint sh "$image" \
    -c 'touch /latch-rootfs-probe 2>/dev/null && exit 1 || exit 0' >/dev/null 2>&1; then
    ok "non-root cannot write the container rootfs"
else
    bad "non-root wrote the container rootfs"
fi

caps="$("$docker" run "${base[@]}" --entrypoint sh "$image" \
    -c 'grep CapEff /proc/self/status' 2>/dev/null || true)"
if printf '%s' "$caps" | grep -q '000001ffffffffff'; then
    bad "container appears privileged (full capability set): $caps"
else
    ok "container capability set is not privileged"
fi

if "$docker" run "${base[@]}" --entrypoint sh "$image" -c \
    'bwrap --die-with-parent --new-session --unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-net --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /run --clearenv --setenv PATH /usr/bin:/bin -- /bin/bash -c "printf INNER_OK"' \
    > "$work/inner-sandbox.log" 2>&1; then
    ok "Bubblewrap sandbox runs inside the container"
else
    bad "Bubblewrap sandbox failed inside the container"; sed 's/^/       /' "$work/inner-sandbox.log" >&2
fi

# --- summary -------------------------------------------------------------
echo
echo "passed: $pass   failed: $fail"

if [ "$fail" -gt 0 ] || [ "${KEEP:-0}" = "1" ]; then
    echo "artifacts kept at $work"
    exit 1
fi
rm -rf "$work"
exit 0
