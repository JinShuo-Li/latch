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
        --run-id) run_id="$2"; shift 2 ;;
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
    always) do_build=1 ;;
    never) do_build=0 ;;
    auto) do_build=$((1 - image_present)) ;;
esac

if [ "$do_build" -eq 1 ]; then
    echo "building $image (target: runtime) ..." >&2
    if [ -n "${CARGO_BUILD_JOBS:-}" ]; then
        "$docker" build --target runtime -t "$image" -f "$root/docker/Dockerfile" \
            --build-arg "CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS" "$root"
    else
        "$docker" build --target runtime -t "$image" -f "$root/docker/Dockerfile" "$root"
    fi
elif [ "$image_present" -eq 0 ]; then
    die "image $image not found; run without --no-build first"
fi

# Latch's mandatory Bubblewrap sandbox needs unprivileged user namespaces;
# Docker's default seccomp and system-path masking block the nested mounts.
# Neither flag is `--privileged`, and the container stays non-root.
declare -a run_args=(
    --rm
    --network "$network"
    --security-opt seccomp=unconfined
    --security-opt systempaths=unconfined
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
    rm -rf "$run_dir"
    echo "removed:   $run_dir" >&2
elif [ "$remove" -eq 0 ]; then
    echo "kept:      $run_dir (use --remove to delete)" >&2
fi

exit "$status"
