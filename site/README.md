# Project website

The GitHub Pages site is static HTML generated from repository Markdown. The
only build dependency is pinned Python-Markdown; the deployed site uses system
fonts, CSS, and vanilla JavaScript, with no CDN or runtime service. Navigation
and article content work without JavaScript. Search loads a local JSON index
and sends no queries to a service.

## Build, check, and preview

Python 3.11+ is required for the site build only:

```sh
python3 -m venv /tmp/latch-site-venv
/tmp/latch-site-venv/bin/pip install -r site/requirements.txt
/tmp/latch-site-venv/bin/python site/build.py
python3 scripts/test_site.py target/site-preview
python3 -m http.server 8000 --directory target/site-preview
```

Open `http://localhost:8000`. `--output <directory>` selects another generated
output directory. Generated HTML and the search index are not committed.
The build stages the authoritative installers from `scripts/`.

Optional browser checks require Node 22+ and a Chromium executable:

```sh
node scripts/test_site.mjs /path/to/chromium target/site-preview
```

These checks cover desktop/mobile layouts, keyboard navigation, local search,
code copying, screenshot tabs, and navigation without JavaScript. No provider
calls or credentials are needed. Screenshots for review land in `target/site-review/`.

## Content ownership

`navigation.json` declares the eight sidebar groups and their ordered pages.
Each page names a repository source. Optional `section` and `through` fields
publish an exact heading range from an existing document. The build fails if
headings or source references disappear. Full documents remain on GitHub; use
`Edit this page` to reach the authoritative source. Write new shared content
in `docs/`, not in a separate website copy.

`templates/home.html` owns the compact landing page; `templates/page.html`
owns the shared shell. The build reads the version from `Cargo.toml`. Keep
installation snippets aligned with `docs/INSTALL.md`. Markdown fences, tables,
heading anchors, lists, links, and repository images are supported. Mermaid
fences are shown as labeled diagram source rather than loading a diagram engine.
Relative document links become local pages where published; other source links
point to GitHub. The output supports GitHub Pages project subpaths.

`.github/workflows/pages.yml` builds and checks the site before uploading it.
Changes to repository Markdown, configuration, version, installers, or the site
trigger deployment. Keep the paths filter in sync with new content sources.

## Interface captures

`assets/terminal.png` renders the actual Ratatui snapshot
`crates/latch-tui/tests/snapshots/v4_wide_sidebar.txt`, mirrored in
`captures/terminal.txt`. It is fixture output, not a live coding session.
Existing text captures remain available. `assets/web.png` shows the production
Web interface running from the local v0.3.2 release binary with an isolated
home, workspace, and state, in the first-run workspace view, before configuring a provider. It contains no credential,
provider result, or prototype content. Captions preserve this distinction.

To refresh both images after UI changes (requires Pillow, Node 22+, Chromium,
and a current release binary):

```sh
python3 scripts/capture_site_terminal.py
node scripts/capture_site_web.mjs /path/to/chromium target/release/latch
```

Review the images and update this provenance and captions when the captured
state changes. Never replace interface captures with generated illustrations.
