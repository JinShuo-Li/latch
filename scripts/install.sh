#!/usr/bin/env bash
# Usage: curl -fsSL https://jinshuo-li.github.io/latch/install.sh | bash
# Pin:   curl -fsSL https://jinshuo-li.github.io/latch/install.sh | bash -s -- --version v0.3.0
set -euo pipefail

main() {
  local version="${LATCH_VERSION:-latest}" install_dir="${LATCH_INSTALL_DIR:-$HOME/.local/bin}"
  local repo='https://github.com/JinShuo-Li/latch'
  latch_install_work=''
  latch_install_staged=''
  trap 'rm -rf -- "$latch_install_work"; if [[ -n "$latch_install_staged" ]]; then rm -f -- "$latch_install_staged"; fi' EXIT
  local os arch release_url base asset expected actual member legacy
  while (( $# )); do
    case "$1" in
      --version|--install-dir)
        if (( $# < 2 )) || [[ -z "$2" ]]; then fail "$1 requires a value"; fi
        if [[ "$1" == --version ]]; then version="$2"; else install_dir="$2"; fi
        shift 2 ;;
      -h|--help)
        echo 'Usage: install.sh [--version vX.Y.Z] [--install-dir DIR]'
        echo 'Defaults: latest stable release, ~/.local/bin. Also accepts LATCH_VERSION and LATCH_INSTALL_DIR.'
        return ;;
      *) fail "Unknown option: $1 (see --help)" ;;
    esac
  done
  os="$(uname -s)"; arch="$(uname -m)"
  [[ "$os" == Linux ]] || fail "Unsupported OS: $os. Binary releases support Linux and Windows; macOS is not supported."
  [[ "$arch" == x86_64 || "$arch" == amd64 ]] || fail "Unsupported architecture: $arch. Linux releases currently require x86_64."
  [[ -n "$install_dir" ]] || fail 'Install directory cannot be empty.'
  for dependency in curl tar sha256sum install mktemp; do
    command -v "$dependency" >/dev/null 2>&1 || fail "Missing $dependency. Install it and retry."
  done
  if [[ "$version" == latest ]]; then
    echo 'Resolving the latest Latch release…'
    release_url="$(curl --proto '=https' --proto-redir '=https' -fsSLI --retry 3 \
      --connect-timeout 15 --max-time 120 -o /dev/null -w '%{url_effective}' "$repo/releases/latest")" \
      || fail "Cannot find the latest release. Check your connection and $repo/releases; use a source build if no binaries are published yet."
    [[ "$release_url" == "$repo/releases/tag/"* ]] || fail 'GitHub did not return a release tag. Try --version vX.Y.Z.'
    version="${release_url##*/}"
  fi
  [[ "$version" == v* ]] || version="v$version"
  [[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || fail "Invalid version: $version. Expected vX.Y.Z (or a prerelease tag)."
  asset='latch-x86_64-unknown-linux-gnu.tar.gz'
  base="$repo/releases/download/$version"
  latch_install_work="$(mktemp -d)"
  echo "Downloading Latch $version for x86_64 Linux…"
  download "$base/SHA256SUMS" "$latch_install_work/SHA256SUMS"
  member=latch
  expected="$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print $1 }' "$latch_install_work/SHA256SUMS")"
  # Older official Linux releases used versioned assets and a top-level directory.
  if [[ -z "$expected" ]]; then
    legacy="latch-$version-x86_64-unknown-linux-gnu"
    asset="$legacy.tar.gz"
    member="$legacy/latch"
    expected="$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print $1 }' "$latch_install_work/SHA256SUMS")"
  fi
  [[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] || fail "Missing or invalid checksum for $asset. Nothing was installed."
  download "$base/$asset" "$latch_install_work/$asset"
  actual="$(sha256sum "$latch_install_work/$asset")"; actual="${actual%% *}"
  [[ "${actual,,}" == "${expected,,}" ]] || fail 'SHA256 mismatch. Nothing was installed; retry or report the release.'
  mkdir "$latch_install_work/extracted"
  if [[ "$member" == latch ]]; then
    tar -xzf "$latch_install_work/$asset" -C "$latch_install_work/extracted" "$member"
  else
    tar -xzf "$latch_install_work/$asset" -C "$latch_install_work/extracted" --strip-components=1 "$member"
  fi || fail 'Release archive does not contain latch.'
  [[ -f "$latch_install_work/extracted/latch" && ! -L "$latch_install_work/extracted/latch" ]] || fail 'Release archive does not contain a regular latch binary.'
  mkdir -p -- "$install_dir" || fail "Cannot create $install_dir. Choose a writable --install-dir."
  [[ ! -d "$install_dir/latch" ]] || fail "$install_dir/latch is a directory. Choose another --install-dir."
  latch_install_staged="$(mktemp "$install_dir/.latch-install.XXXXXXXX")"
  install -m 755 "$latch_install_work/extracted/latch" "$latch_install_staged"
  mv -fT -- "$latch_install_staged" "$install_dir/latch" || fail "Cannot replace $install_dir/latch. Check directory permissions."
  latch_install_staged=''
  echo "Installed Latch $version to $install_dir/latch (SHA256 verified)."
  # Print a literal $PATH for the shell profile.
  # shellcheck disable=SC2016
  case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) printf 'Add this directory to PATH in your shell profile:\n  export PATH=%q:"$PATH"\n' "$install_dir" ;;
  esac
  echo 'Runtime: Linux with glibc 2.35+, Bubblewrap (bwrap), ripgrep (rg), and Git.'
  echo 'Next: run latch, use /setup to configure a provider, then run latch doctor.'
}

fail() { echo "Latch installer: $*" >&2; exit 1; }
download() {
  curl --proto '=https' --proto-redir '=https' -fsSL --retry 3 --connect-timeout 15 --max-time 300 \
    "$1" -o "$2" || fail "Cannot download $1. Check the release exists and your connection. Nothing was installed."
}

# Keep invocation last so a partially downloaded script does not start installing.
main "$@"
