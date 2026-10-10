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
   git tag v0.3.2
   git push origin v0.3.2
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

[Pages](../.github/workflows/pages.yml) builds static HTML from repository Markdown,
checks local links and browser behavior, and stages the two authoritative
installers from `scripts/` before uploading `_site/`. Source documentation,
version, configuration, installer, and website edits trigger deployment.
Do not maintain duplicate installer copies in `site/`.

For a local preview with the same generated layout:

```sh
python3 -m venv /tmp/latch-site-venv
/tmp/latch-site-venv/bin/pip install -r site/requirements.txt
/tmp/latch-site-venv/bin/python site/build.py
python3 scripts/test_site.py target/site-preview
python3 -m http.server 8000 --directory target/site-preview
```

The site uses system fonts, CSS, and vanilla JavaScript. Python 3.11+ and a
pinned Markdown parser are build-only requirements; no frontend framework or
runtime server is deployed. Version labels come from `Cargo.toml`.
See the [site maintenance guide](../site/README.md) for content sources,
search, optional browser checks, and genuine interface capture provenance.
