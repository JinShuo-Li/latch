#!/usr/bin/env python3
"""Unit checks for site/build.py: output guard and code-safe link rewriting.

Requires the site build requirements (Python-Markdown); the Pages workflow
installs them before running this check.
"""
import importlib.util
from pathlib import Path
import sys
import tempfile

# Importing site/build.py must not leave a __pycache__ in the repository.
sys.dont_write_bytecode = True

REPO = Path(__file__).resolve().parent.parent

spec = importlib.util.spec_from_file_location('site_build', REPO / 'site' / 'build.py')
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)

failures = []


def check(condition, message):
    if not condition:
        failures.append(message)


# Output guard: the repository, its parents, and tracked trees are refused.
for rejected in (
    REPO,
    REPO.parent,
    build.SITE,
    REPO / 'docs',
    REPO / 'docs' / 'nested',
    REPO / 'site' / 'generated',
):
    try:
        build.validate_output(rejected)
    except ValueError:
        pass
    else:
        failures.append(f'{rejected} must be refused as build output')

# Generated directories and paths outside the repository stay allowed.
with tempfile.TemporaryDirectory() as outside:
    for allowed in (REPO / 'target' / 'site-preview', REPO / '_site', Path(outside) / 'site'):
        try:
            resolved = build.validate_output(allowed)
        except ValueError as error:
            failures.append(f'{allowed} must stay allowed: {error}')
        else:
            check(resolved.is_absolute(), f'{allowed} must resolve to an absolute path')

# The real build refuses a tracked output before writing anything.
try:
    build.build(REPO / 'docs')
except ValueError:
    pass
else:
    failures.append('build() must refuse --output docs')
check(not (REPO / 'docs' / 'index.html').exists(), 'refused build must not write into docs/')

# Link rewriting: only links outside code regions reach the rewriter.
seen = []


def rewriter(value):
    seen.append(value)
    return 'REWRITTEN'


plain = build.rewrite_links('<p><a href="page.md">x</a></p>', rewriter)
check(plain == '<p><a href="REWRITTEN">x</a></p>', f'unexpected rewrite: {plain}')
check(seen == ['page.md'], f'rewriter calls: {seen}')

import markdown  # noqa: E402  (import after the pure checks for a clear failure)

indented = markdown.markdown('    <a href="x">y</a>\n', extensions=['fenced_code'])
seen.clear()
kept = build.rewrite_links(indented, rewriter)
check('href="x"' in kept, f'indented code sample was rewritten: {kept}')
check(seen == [], f'indented code sample reached the rewriter: {seen}')

inline = markdown.markdown('`<a href="x">y</a>`\n', extensions=['fenced_code'])
seen.clear()
kept = build.rewrite_links(inline, rewriter)
check('href="x"' in kept, f'inline code sample was rewritten: {kept}')
check(seen == [], f'inline code sample reached the rewriter: {seen}')

fenced = markdown.markdown('```html\n<a href="x">y</a>\n```\n', extensions=['fenced_code'])
seen.clear()
kept = build.rewrite_links(fenced, rewriter)
check('href=&quot;x&quot;' in kept, f'fenced sample changed: {kept}')
check(seen == [], f'fenced code sample reached the rewriter: {seen}')

raw = build.rewrite_links(
    '<pre><code>href="x"</code></pre><p><img src="real.png"></p>', rewriter)
check('href="x"' in raw and 'src="REWRITTEN"' in raw, f'raw HTML handling wrong: {raw}')
check(seen == ['real.png'], f'raw HTML rewriter calls: {seen}')

seen.clear()
mixed = build.rewrite_links(
    '<p><a href="one.md">1</a></p><pre><code>href="two"</code></pre><p><code>src="three"</code></p>',
    rewriter,
)
check(seen == ['one.md'], f'mixed rewriter calls: {seen}')
check('href="two"' in mixed and 'src="three"' in mixed, f'mixed content changed: {mixed}')

if failures:
    raise SystemExit('\n'.join(failures))
print('site build self-check: output guard and code-safe link rewriting passed')
