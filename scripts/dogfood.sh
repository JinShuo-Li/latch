#!/usr/bin/env bash
#
# Run one Latch task against a disposable workspace inside an isolated Docker
# container.
#
#   ./scripts/dogfood.sh "Fix the bug in this fixture and run its tests"
#
# The container runs the existing machine CLI (`latch run ...`); this script
# only prepares the image, the workspace, the mounts, and the explicitly
# forwarded environment. See docs/DOGFOOD_DOCKER.md.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

image="${LATCH_DOGFOOD_IMAGE:-latch-dogfood:local}"
docker="${DOCKER:-docker}"
config="$root/docker/config.dogfood.toml"
network="${LATCH_DOGFOOD_NETWORK:-bridge}"
output="text"
mode=""
provider=""
model=""
workspace=""
fixture=""
run_id=""
run_id_set=0
prompt=""
build="auto" # auto | always | never
remove=0
shell=0
forward_env=()

usage() {
    cat <<'EOF'
Usage: scripts/dogfood.sh [options] [--] "<prompt>"

Runs one Latch task in a disposable Docker workspace.

Options:
  --prompt TEXT          Task prompt (also accepted as the trailing argument).
  --workspace DIR        Mount this host directory read-write as the workspace.
                         Without it a disposable workspace is created.
  --fixture DIR          Seed a disposable workspace from this host directory.
  --config FILE          Host config to mount at /config/config.toml.
                         Default: docker/config.dogfood.toml
  --provider-env NAME    Forward host env var NAME into the container (repeatable).
                         Use this for provider credentials; nothing else is forwarded.
  --model MODEL          Latch --model override.
  --provider PROVIDER    Latch --provider override.
  --mode MODE            Latch --mode override (ask|plan|work).
  --output FORMAT        Latch --output format (text|json|jsonl).
  --network MODE         Docker network (default bridge; use none for offline).
  --image NAME           Image tag (default latch-dogfood:local).
  --build                Force a rebuild.
  --no-build             Never build; fail if the image is missing.
  --run-id ID            Run directory name under target/dogfood/.
  --shell                Drop into an interactive shell in the container.
  --keep                 Keep the run directory (the default).
  --remove               Delete the run directory when finished.
  -h, --help             Show this help.

Outputs and state are kept under target/dogfood/<run-id>/ so the resulting
workspace, logs, and diff can be inspected from the host.
EOF
}

die() {
    echo "error: $*" >&2
    exit 2
}

# Run-id validation and the recursive-deletion guard live in a shared library
# so scripts/dogfood-test.sh can cover them deterministically without Docker.
# shellcheck source=dogfood-lib.sh
. "$root/scripts/dogfood-lib.sh"

validate_run_id() {
    local reason
    reason="$(dogfood_run_id_error "$1")"
    [ -z "$reason" ] || die "$reason"
}

safe_remove_run_dir() {
    dogfood_safe_remove "$root/target/dogfood" "$1"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prompt) prompt="$2"; shift 2 ;;
        --workspace) workspace="$2"; shift 2 ;;
        --fixture) fixture="$2"; shift 2 ;;
        --config) config="$2"; shift 2 ;;
        --provider-env) forward_env+=("$2"); shift 2 ;;
        --model) model="$2"; shift 2 ;;
        --provider) provider="$2"; shift 2 ;;
        --mode) mode="$2"; shift 2 ;;
        --output) output="$2"; shift 2 ;;
        --network) network="$2"; shift 2 ;;
        --image) image="$2"; shift 2 ;;
        --build) build="always"; shift ;;
        --no-build) build="never"; shift ;;
        --run-id) run_id="$2"; run_id_set=1; shift 2 ;;
        --shell) shell=1; shift ;;
        --keep) shift ;;
        --remove) remove=1; shift ;;
        -h|--help) usage; exit 0 ;;
        --) shift; break ;;
        -*) die "unknown option: $1 (see --help)" ;;
        *) break ;;
    esac
done

if [ -z "$prompt" ] && [ $# -gt 0 ]; then
    prompt="$*"
fi

# Reject a hostile or accidental run id before touching Docker or the
# filesystem. Deletion is guarded again by safe_remove_run_dir below.
if [ "$run_id_set" -eq 1 ]; then
    validate_run_id "$run_id"
fi

# Source provenance: the exact revision the image must correspond to. A dirty
# tree is recorded explicitly so a result is never silently attributed to a
# clean commit.
if git -C "$root" rev-parse --verify HEAD >/dev/null 2>&1; then
    source_commit="$(git -C "$root" rev-parse HEAD)"
    if [ -n "$(git -C "$root" status --porcelain 2>/dev/null)" ]; then
        source_commit="${source_commit}-dirty"
    fi
else
    source_commit="unknown"
fi

if ! command -v "$docker" >/dev/null 2>&1; then
    die "docker is not installed or not on PATH"
fi
if [ ! -f "$config" ]; then
    die "config not found: $config"
fi
config="$(cd "$(dirname "$config")" && pwd)/$(basename "$config")"

if [ "$shell" -eq 0 ] && [ -z "$prompt" ]; then
    die "missing prompt: pass it as an argument or with --prompt"
fi

host_uid="$(id -u)"
host_gid="$(id -g)"
if [ "$host_uid" -eq 0 ]; then
    echo "warning: running as root; using the image's non-root latch user (1000):1000" >&2
    host_uid=1000
    host_gid=1000
fi

if [ -n "$run_id" ]; then
    run_dir="$root/target/dogfood/$run_id"
else
    run_dir="$root/target/dogfood/$(date +%Y%m%d-%H%M%S)-$$"
fi

if [ -n "$workspace" ]; then
    [ -d "$workspace" ] || die "workspace is not a directory: $workspace"
    workspace="$(cd "$workspace" && pwd -P)"
    disposable=0
    mkdir -p "$run_dir"
else
    workspace="$run_dir/workspace"
    mkdir -p "$workspace"
    if [ -n "$fixture" ]; then
        [ -d "$fixture" ] || die "fixture is not a directory: $fixture"
        cp -a "$fixture/." "$workspace/"
    fi
    disposable=1
fi

state_dir="$run_dir/state"
home_dir="$run_dir/home"
out_dir="$run_dir/out"
mkdir -p "$state_dir" "$home_dir" "$out_dir"

image_present=0
if "$docker" image inspect "$image" >/dev/null 2>&1; then
    image_present=1
fi

case "$build" in
    # `auto` rebuilds from the current source every time: "image exists" does
    # not mean "image is current". BuildKit decides which layers are reusable,
    # so source-only changes stay cheap while the image provenance always
    # tracks the current tree.
    always|auto) do_build=1 ;;
    never) do_build=0 ;;
esac

if [ "$do_build" -eq 1 ]; then
    echo "building $image (target: runtime, revision: $source_commit) ..." >&2
    declare -a build_args=(
        build --target runtime -t "$image" -f "$root/docker/Dockerfile"
        --build-arg "LATCH_SOURCE_REVISION=$source_commit"
    )
    if [ -n "${CARGO_BUILD_JOBS:-}" ]; then
        build_args+=(--build-arg "CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS")
    fi
    "$docker" "${build_args[@]}" "$root"
elif [ "$image_present" -eq 0 ]; then
    die "image $image not found; run without --no-build first"
fi

# Provenance: read back what the image actually says it was built from, so a
# stale or unknown image can never be mistaken for the current source.
image_commit="$("$docker" image inspect \
    --format '{{ index .Config.Labels "org.opencontainers.image.revision" }}' \
    "$image" 2>/dev/null || true)"
image_commit="${image_commit:-unknown}"
if [ "$image_commit" != "$source_commit" ]; then
    if [ "$do_build" -eq 1 ]; then
        die "built image provenance ($image_commit) does not match source ($source_commit)"
    fi
    echo "warning: image $image records '$image_commit' but source is '$source_commit';" >&2
    echo "         --no-build reuses a possibly stale image and results may be invalid" >&2
fi

# Record provenance in the run directory so every result is attributable to an
# exact source revision and image. `image_commit == source_commit` (including
# the `-dirty` suffix) is the harness's trust precondition.
dirty=false
case "$source_commit" in *-dirty) dirty=true ;; esac
cat > "$run_dir/provenance.json" <<EOF
{
  "source_commit": "$source_commit",
  "image_commit": "$image_commit",
  "image": "$image",
  "dirty": $dirty,
  "built": $([ "$do_build" -eq 1 ] && echo true || echo false),
  "provenance_ok": $([ "$image_commit" = "$source_commit" ] && echo true || echo false)
}
EOF

# Latch's mandatory Bubblewrap sandbox needs unprivileged user namespaces;
# Docker's default seccomp and system-path masking block the nested mounts.
# Neither flag is `--privileged`, and the container stays non-root. On top of
# that baseline the harness drops all Linux capabilities, forbids privilege
# escalation, and caps the process count; none of these interferes with
# unprivileged nested Bubblewrap (verified by scripts/dogfood-test.sh).
declare -a run_args=(
    --rm
    --network "$network"
    --security-opt seccomp=unconfined
    --security-opt systempaths=unconfined
    --security-opt no-new-privileges:true
    --cap-drop ALL
    --pids-limit "${LATCH_DOGFOOD_PIDS_LIMIT:-512}"
    --user "$host_uid:$host_gid"
    -v "$workspace:/workspace"
    -v "$state_dir:/state"
    -v "$home_dir:/home/latch"
    -v "$config:/config/config.toml:ro"
    -e HOME=/home/latch
)

if [ "${#forward_env[@]}" -gt 0 ]; then
    for name in "${forward_env[@]}"; do
        if [ -z "${!name:-}" ]; then
            echo "warning: --provider-env $name is not set in the host environment" >&2
        fi
        run_args+=(-e "$name")
    done
fi

if [ "$shell" -eq 1 ]; then
    run_args+=(--entrypoint /bin/bash -it)
    echo "workspace: $workspace" >&2
    echo "state:     $state_dir" >&2
    exec "$docker" run "${run_args[@]}" "$image" -l
fi

declare -a command=(run --config /config/config.toml --workspace /workspace \
    --prompt "$prompt" --output "$output")
[ -n "$provider" ] && command+=(--provider "$provider")
[ -n "$model" ] && command+=(--model "$model")
[ -n "$mode" ] && command+=(--mode "$mode")

set +e
"$docker" run "${run_args[@]}" "$image" "${command[@]}" \
    > "$out_dir/stdout.log" 2> "$out_dir/stderr.log"
status=$?
set -e

cat "$out_dir/stdout.log"
cat "$out_dir/stderr.log" >&2

echo "---" >&2
echo "exit:      $status" >&2
echo "workspace: $workspace" >&2
echo "state:     $state_dir" >&2
echo "logs:      $out_dir" >&2
if [ "$disposable" -eq 1 ] && [ -d "$workspace/.git" ]; then
    echo "diff:      git -C $workspace diff" >&2
fi

if [ "$remove" -eq 1 ] && [ "$disposable" -eq 1 ]; then
    safe_remove_run_dir "$run_dir"
    echo "removed:   $run_dir" >&2
elif [ "$remove" -eq 0 ]; then
    echo "kept:      $run_dir (use --remove to delete)" >&2
fi

exit "$status"
