#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fetch_reference() {
  local name="$1"
  local remote="$2"
  local revision destination current shallow
  revision="$(tr -d '[:space:]' < "$root/references/$name.rev")"
  destination="$root/.references/$name"

  if [[ -e "$destination" ]]; then
    current="$(git -C "$destination" rev-parse HEAD)"
    shallow="$(git -C "$destination" rev-parse --is-shallow-repository)"
    if [[ "$current" == "$revision" && "$shallow" == "true" ]]; then
      chmod -R a-w "$destination"
      return
    fi
    echo "refusing to replace existing reference checkout: $destination" >&2
    return 1
  fi

  mkdir -p "$root/.references"
  git init --quiet "$destination"
  git -C "$destination" remote add origin "$remote"
  git -C "$destination" fetch --quiet --depth 1 --no-tags origin "$revision"
  git -C "$destination" checkout --quiet --detach FETCH_HEAD

  test "$(git -C "$destination" rev-parse HEAD)" = "$revision"
  test "$(git -C "$destination" rev-parse --is-shallow-repository)" = "true"
  chmod -R a-w "$destination"
}

fetch_reference pi https://github.com/earendil-works/pi.git
fetch_reference codex https://github.com/openai/codex.git
