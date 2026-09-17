#!/usr/bin/env bash
# Shared dogfood-runner helpers.
#
# Sourced by `scripts/dogfood.sh` and by `scripts/dogfood-test.sh`. Keeping the
# run-id validation and the recursive-deletion guard here lets the deterministic
# harness exercise them directly, without Docker and without a real run.

# Echo the reason a run id is invalid, or nothing when it is valid.
#
# A run id names exactly one directory directly under target/dogfood/. It is
# restricted to `^[A-Za-z0-9][A-Za-z0-9._-]*$` so `../`, absolute paths, path
# separators, and shell/control characters can never escape the dogfood root.
dogfood_run_id_error() {
    case "$1" in
        "") printf '%s' "--run-id must not be empty" ;;
        [A-Za-z0-9]*)
            case "$1" in
                *..*) printf '%s' "invalid --run-id (contains '..'): $1" ;;
                *[!A-Za-z0-9._-]*) printf '%s' "invalid --run-id (allowed: A-Za-z0-9._-): $1" ;;
                *) printf '%s' "" ;;
            esac
            ;;
        *) printf '%s' "invalid --run-id (must start alphanumeric; allowed: A-Za-z0-9._-): $1" ;;
    esac
}

# Recursively delete `$2` only when its fully resolved path is a descendant of
# the dogfood root `$1`. Prints a warning and deletes nothing otherwise. This is
# defense in depth behind `dogfood_run_id_error`: it also catches a symlink
# planted inside the root that points outside it.
dogfood_safe_remove() {
    local root="$1" target="$2" root_real target_real
    [ -e "$target" ] || return 0
    root_real="$(cd "$root" 2>/dev/null && pwd -P)" || {
        echo "warning: refusing to delete without a resolvable dogfood root: $target" >&2
        return 0
    }
    target_real="$(cd "$target" 2>/dev/null && pwd -P)" || return 0
    case "$target_real" in
        "$root_real"/*) rm -rf "$target_real" ;;
        *) echo "refusing to delete outside $root_real: $target_real" >&2 ;;
    esac
}
