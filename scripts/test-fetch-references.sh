#!/usr/bin/env bash
# Offline self-check for scripts/fetch-references.sh. Local upstream
# repositories mean no network access is required. Covers the success path,
# recovery from a half-initialized checkout, refusal to replace real checkouts,
# and staging cleanup after a failed fetch.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d)"
cleanup() {
  chmod -R u+w "$work" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

passed=0
fail() {
  echo "FAIL: $*" >&2
  exit 1
}
ok() {
  passed=$((passed + 1))
  echo "ok $passed - $*"
}

# Local upstream with one commit. Fetching an exact SHA needs the
# reachable-SHA allowance that real hosts provide.
upstream="$work/upstream"
git init --quiet --bare "$upstream"
git -C "$upstream" config uploadpack.allowReachableSHA1InWant true
seed="$work/seed"
git init --quiet "$seed"
git -C "$seed" -c user.email=t@l -c user.name=t commit --quiet --allow-empty -m init
revision="$(git -C "$seed" rev-parse HEAD)"
git -C "$seed" push --quiet "$upstream" HEAD:refs/heads/main

# A repository-shaped fixture around the real script that fetches only the
# local upstream.
repo="$work/repo"
mkdir -p "$repo/scripts" "$repo/references"
script="$repo/scripts/fetch-references.sh"
grep -v -e '^fetch_reference pi ' -e '^fetch_reference codex ' \
  "$script_dir/fetch-references.sh" > "$script"
printf 'fetch_reference pi %s\n' "$upstream" >> "$script"
printf '%s\n' "$revision" > "$repo/references/pi.rev"

checkout="$repo/.references/pi"
staging_leftovers() {
  find "$repo/.references" -maxdepth 1 -name '.*.partial.*' -print -quit 2>/dev/null
}

# 1. Fresh fetch materializes the pinned shallow revision.
bash "$script" >/dev/null 2>&1 || fail "fresh fetch failed"
[[ "$(git -C "$checkout" rev-parse HEAD)" == "$revision" ]] \
  || fail "fresh fetch checked out the wrong revision"
[[ "$(git -C "$checkout" rev-parse --is-shallow-repository)" == "true" ]] \
  || fail "fresh checkout is not shallow"
ok "fresh fetch materializes the pinned revision"

# 2. Rerun is idempotent and leaves no staging directories.
bash "$script" >/dev/null 2>&1 || fail "idempotent rerun failed"
[[ -z "$(staging_leftovers)" ]] || fail "staging directory left behind"
ok "rerun keeps the verified checkout"

# 3. A half-initialized checkout from a failed fetch is recovered.
chmod -R u+w "$checkout"
rm -rf "$checkout"
mkdir -p "$checkout"
git init --quiet "$checkout"
bash "$script" >/dev/null 2>&1 || fail "recovery from a poisoned checkout failed"
[[ "$(git -C "$checkout" rev-parse HEAD)" == "$revision" ]] \
  || fail "recovery checked out the wrong revision"
ok "incomplete checkout is removed and refetched"

# 4. A checkout with real content but no HEAD is refused, never deleted.
chmod -R u+w "$checkout"
rm -rf "$checkout"
mkdir -p "$checkout"
git init --quiet "$checkout"
printf 'user work\n' > "$checkout/notes.txt"
if bash "$script" >/dev/null 2>"$work/refuse.log"; then
  fail "a checkout with real content must be refused"
fi
[[ -f "$checkout/notes.txt" ]] || fail "refusal must not delete content"
grep -q "no valid HEAD" "$work/refuse.log" || fail "refusal message must explain the fix"
ok "checkout with unknown content is refused, not deleted"

# 5. A valid checkout at a different revision is refused.
chmod -R u+w "$checkout"
rm -rf "$checkout"
git init --quiet "$checkout"
git -C "$checkout" -c user.email=t@l -c user.name=t commit --quiet --allow-empty -m other
if bash "$script" >/dev/null 2>"$work/replace.log"; then
  fail "an existing checkout at another revision must be refused"
fi
grep -q "refusing to replace" "$work/replace.log" \
  || fail "replacement refusal message missing"
ok "existing checkout is never replaced"

# 6. A failed fetch leaves no poisoned destination and no staging directory.
chmod -R u+w "$checkout"
rm -rf "$checkout"
printf '%s\n' '0000000000000000000000000000000000000000' > "$repo/references/pi.rev"
if bash "$script" >/dev/null 2>&1; then
  fail "an unresolvable revision must fail"
fi
[[ ! -e "$checkout" ]] || fail "failed fetch must not create a destination"
[[ -z "$(staging_leftovers)" ]] || fail "failed fetch left a staging directory"
ok "failed fetch leaves no poisoned state"

echo "fetch-references self-check: $passed/6 passed"
