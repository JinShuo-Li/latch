#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
revision="$(tr -d '[:space:]' < "$root/references/pi.rev")"
destination="$root/.references/pi"

if [[ -e "$destination" ]]; then
  current="$(git -C "$destination" rev-parse HEAD)"
  shallow="$(git -C "$destination" rev-parse --is-shallow-repository)"
  if [[ "$current" == "$revision" && "$shallow" == "true" ]]; then
    exit 0
  fi
  echo "refusing to replace existing reference checkout: $destination" >&2
  exit 1
fi

mkdir -p "$root/.references"
git init --quiet "$destination"
git -C "$destination" remote add origin https://github.com/earendil-works/pi.git
git -C "$destination" fetch --quiet --depth 1 --no-tags origin "$revision"
git -C "$destination" checkout --quiet --detach FETCH_HEAD

test "$(git -C "$destination" rev-parse HEAD)" = "$revision"
test "$(git -C "$destination" rev-parse --is-shallow-repository)" = "true"
