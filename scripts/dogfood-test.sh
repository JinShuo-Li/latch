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

# Shared run-id validation and deletion guard, exercised directly below.
# shellcheck source=dogfood-lib.sh
. "$root/scripts/dogfood-lib.sh"

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

# Source provenance for the image this test builds and runs.
if git -C "$root" rev-parse --verify HEAD >/dev/null 2>&1; then
    source_commit="$(git -C "$root" rev-parse HEAD)"
    if [ -n "$(git -C "$root" status --porcelain 2>/dev/null)" ]; then
        source_commit="${source_commit}-dirty"
    fi
else
    source_commit="unknown"
fi

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
    # `auto` always rebuilds from the current source; BuildKit cache keeps a
    # source-only rebuild cheap. `--no-build` is the explicit stale-image path.
    always|auto) do_build=1 ;;
    never) do_build=0 ;;
esac

if [ "$do_build" -eq 1 ]; then
    echo "building $image (target: test, revision: $source_commit) ..."
    if "$docker" build --target test -t "$image" -f "$root/docker/Dockerfile" \
        --build-arg "LATCH_SOURCE_REVISION=$source_commit" "$root"; then
        ok "image build (target: test)"
    else
        bad "image build (target: test)"
        echo "cannot continue without an image" >&2
        exit 1
    fi
else
    ok "reused existing image $image (--no-build)"
fi

# The image must identify the exact source revision it was built from.
image_commit="$("$docker" image inspect \
    --format '{{ index .Config.Labels "org.opencontainers.image.revision" }}' \
    "$image" 2>/dev/null || true)"
image_commit="${image_commit:-unknown}"
if [ "$image_commit" = "$source_commit" ]; then
    ok "image provenance matches source revision ($image_commit)"
else
    bad "image provenance '$image_commit' != source '$source_commit'"
fi

# Latch's mandatory Bubblewrap sandbox needs unprivileged user namespaces;
# Docker's default seccomp and system-path masking block the nested mounts.
# On top of that compatibility baseline the harness drops all capabilities,
# forbids privilege escalation, and caps the process count.
declare -a base=(
    --rm
    --network none
    --security-opt seccomp=unconfined
    --security-opt systempaths=unconfined
    --security-opt no-new-privileges:true
    --cap-drop ALL
    --pids-limit "${LATCH_DOGFOOD_PIDS_LIMIT:-512}"
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
if printf '%s' "$caps" | grep -q '0000000000000000'; then
    ok "capability set is empty (--cap-drop ALL): $caps"
else
    bad "expected an empty capability set with --cap-drop ALL: $caps"
fi

nnp="$("$docker" run "${base[@]}" --entrypoint sh "$image" \
    -c 'grep NoNewPrivs /proc/self/status' 2>/dev/null || true)"
if printf '%s' "$nnp" | grep -q 'NoNewPrivs:.*1'; then
    ok "no-new-privileges is enforced (NoNewPrivs=1)"
else
    bad "no-new-privileges not enforced: $nnp"
fi

if "$docker" run "${base[@]}" --entrypoint sh "$image" -c \
    'bwrap --die-with-parent --new-session --unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-net --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /run --clearenv --setenv PATH /usr/bin:/bin -- /bin/bash -c "printf INNER_OK"' \
    > "$work/inner-sandbox.log" 2>&1; then
    ok "Bubblewrap sandbox runs inside the container"
else
    bad "Bubblewrap sandbox failed inside the container"; sed 's/^/       /' "$work/inner-sandbox.log" >&2
fi

# --- run-id hardening (no Docker required) ------------------------------
# The same library the runner uses is exercised directly.
guard_root="$root/target/dogfood"
mkdir -p "$guard_root"
for bad in '' '..' '../tmp' 'a/b' '/foo' 'a b' 'a..b' '-x' 'a;rm -rf /' 'a$b'; do
    if [ -n "$(dogfood_run_id_error "$bad")" ]; then
        ok "run id rejected: '$bad'"
    else
        bad "run id accepted but must be rejected: '$bad'"
    fi
done
for good in 'run-1' 'a.b_c-9' 'ABC123' 'a.'; do
    if [ -z "$(dogfood_run_id_error "$good")" ]; then
        ok "run id accepted: '$good'"
    else
        bad "run id rejected but must be accepted: '$good'"
    fi
done

# The runner itself rejects a hostile id before touching Docker or the
# filesystem (exit 2 with an explicit message) for every escape shape the
# task calls out.
for bad in '../../tmp' '/foo' '..' 'a/b'; do
    escape_out="$("$root/scripts/dogfood.sh" --run-id "$bad" --prompt x 2>&1 || true)"
    case "$escape_out" in
        *"invalid --run-id"*) ok "runner rejects --run-id '$bad'" ;;
        *) bad "runner did not reject --run-id '$bad': $escape_out" ;;
    esac
done

# Deletion guard: a symlink inside the dogfood root that points outside it must
# never cause the outside directory to be removed.
guard_outside="$work/guard-outside"
mkdir -p "$guard_outside"
touch "$guard_outside/keep.txt"
ln -sfn "$guard_outside" "$guard_root/latch-escape-link"
dogfood_safe_remove "$guard_root" "$guard_root/latch-escape-link" >/dev/null 2>&1 || true
if [ -e "$guard_outside/keep.txt" ]; then
    ok "recursive deletion refuses a symlink escaping the dogfood root"
else
    bad "recursive deletion escaped the dogfood root"
fi
rm -f "$guard_root/latch-escape-link"

# A real descendant is removed as expected.
mkdir -p "$guard_root/latch-removable/child"
dogfood_safe_remove "$guard_root" "$guard_root/latch-removable" >/dev/null 2>&1 || true
if [ ! -e "$guard_root/latch-removable" ]; then
    ok "recursive deletion removes a real dogfood descendant"
else
    bad "recursive deletion left a real dogfood descendant"
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
