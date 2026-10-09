# Binary releases

[Release](../.github/workflows/release.yml) runs when a tag matching `v*.*.*`
is pushed. Manual dispatch runs the same builds and checks without publishing,
so maintainers can verify packaging before creating a tag. It verifies that the tag matches `workspace.package.version`, builds
`latch` on Linux and Windows, runs platform sandbox smoke checks and offline
installer tests, and checks the packaged executable's version. Existing CI
and the manual full-validation workflow remain separate.

## Cut a release

1. Update the workspace version in `Cargo.toml` and regenerate `Cargo.lock`.
   Keep the README, site version labels, and installation documentation aligned.
2. Run `bash scripts/release-gate.sh` on Linux and the Windows full-validation
   workflow. Run the installer checks listed below. Commit the intended files.
3. Push the version commit, then create and push the matching tag, for example:

   ```sh
   git tag v0.3.1
   git push origin v0.3.1
   ```

4. Watch the Release workflow. After both platform jobs succeed, it creates a
   draft, uploads all assets and `SHA256SUMS`, and publishes the complete release.
   Stable tags become latest; tags with a prerelease suffix are marked prerelease
   and do not change the stable install target.

Creating a tag is an explicit publishing action. Ordinary pushes to `main`
publish the Pages site but do not create binary releases.

## Assets and portability

- `latch-x86_64-unknown-linux-gnu.tar.gz`: built on Ubuntu 22.04 for glibc 2.35+.
  SQLite is bundled and TLS uses rustls. The binary is stripped; glibc remains a
  runtime dependency. Bubblewrap, ripgrep, and Git remain system prerequisites.
- `latch-x86_64-pc-windows-msvc.zip`: built on the native x64 MSVC runner with
  `-C target-feature=+crt-static`. The embedded sandbox executable and DLL already
  use `/MT`. Consumers do not need the compiler, Windows SDK, or WSL.
- Each archive contains the executable, `LICENSE`, `config.example.toml`, and
  `BUILD_INFO` (version, target, source commit, and Rust compiler version).
- `install.sh`, `install.ps1`: the installers from the tagged commit.
- `SHA256SUMS`: SHA256 of both archives and the two installer scripts, with bare
  asset filenames. Installers require one exact matching manifest entry.

Only the publish job receives `contents: write`; build jobs cannot publish.
Artifacts are retained for seven days. A failure during upload leaves a draft,
so `latest` installers cannot see a partial release. If publication fails,
inspect and delete the incomplete draft before rerunning the workflow; it will
not overwrite an already published release. GitHub Actions must be allowed to
create releases with `GITHUB_TOKEN` in the repository settings.

## Installer and Pages checks

```sh
bash -n scripts/install.sh
shellcheck scripts/install.sh
python3 scripts/test_install.py
```

On Windows, run:

```powershell
pwsh -NoProfile -File scripts/test-install.ps1
powershell -NoProfile -File scripts/test-install.ps1
```

The tests use real fixture archives and mocked GitHub downloads. They check
latest/pinned selection, checksums, failures preserving existing installs,
archive contents, architecture rejection, and temporary-file cleanup. They
make no network requests and do not edit PATH or user configuration.

[Pages](../.github/workflows/pages.yml) stages `site/` plus the two installers in
`_site/` before uploading. The scripts in `scripts/` are authoritative; do not
maintain duplicate copies in `site/`. Installer edits trigger Pages deployment.
For a local preview with the same layout:

```sh
mkdir -p target/site-preview
cp -R site/. target/site-preview/
cp scripts/install.sh scripts/install.ps1 target/site-preview/
python3 -m http.server 8000 --directory target/site-preview
```

The site uses system fonts, CSS, and a small vanilla JavaScript file. No frontend
build step or package installation is needed. The TUI captures remain copied
from deterministic repository fixtures; keep the inline session fallback in
`site/index.html` aligned with `site/captures/session.txt`. Benchmark values come
from the
[recorded 25-case comparison](../benchmark/reports/2026-10-06-full-comparison/README.md).
