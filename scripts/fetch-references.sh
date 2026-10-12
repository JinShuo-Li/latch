#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fetch_reference() {
  local name="$1"
  local remote="$2"
  local revision destination current shallow staging
  revision="$(tr -d '[:space:]' < "$root/references/$name.rev")"
  destination="$root/.references/$name"

  if [[ -e "$destination" ]]; then
    if current="$(git -C "$destination" rev-parse --verify HEAD 2>/dev/null)"; then
      shallow="$(git -C "$destination" rev-parse --is-shallow-repository)"
      if [[ "$current" == "$revision" && "$shallow" == "true" ]]; then
        chmod -R a-w "$destination"
        return
      fi
      echo "refusing to replace existing reference checkout: $destination" >&2
      return 1
    fi
    # A fetch that failed after `git init` leaves an initialized repository with
    # no HEAD. Treat that empty scaffold as incomplete instead of dying on
    # `rev-parse HEAD` forever; never remove anything that might hold real work.
    if [[ -d "$destination/.git" && -z "$(find "$destination" -mindepth 1 -maxdepth 1 ! -name .git -print -quit)" ]]; then
      echo "removing incomplete reference checkout: $destination" >&2
      rm -rf "$destination"
    else
      echo "cannot use $destination: it has no valid HEAD; remove it manually and rerun" >&2
      return 1
    fi
  fi

  mkdir -p "$root/.references"
  # Fetch into a staging directory and rename into place only after the exact
  # revision is verified, so a failed or interrupted fetch never poisons the
  # destination for later runs. The EXIT trap also fires when `set -e` aborts
  # the script mid-fetch.
  staging="$(mktemp -d "$root/.references/.$name.partial.XXXXXX")"
  # The EXIT trap must not depend on function-local scope: `set -e` unwinds the
  # function before the trap runs, so the path is captured in the trap text.
  # shellcheck disable=SC2064
  trap "rm -rf $(printf '%q' "$staging")" EXIT
  git init --quiet "$staging"
  git -C "$staging" remote add origin "$remote"
  git -C "$staging" fetch --quiet --depth 1 --no-tags origin "$revision"
  git -C "$staging" checkout --quiet --detach FETCH_HEAD

  test "$(git -C "$staging" rev-parse HEAD)" = "$revision"
  test "$(git -C "$staging" rev-parse --is-shallow-repository)" = "true"
  mv "$staging" "$destination"
  trap - EXIT
  chmod -R a-w "$destination"
}

fetch_reference pi https://github.com/earendil-works/pi.git
fetch_reference codex https://github.com/openai/codex.git
