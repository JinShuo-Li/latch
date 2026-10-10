#!/usr/bin/env python3
"""Build static Pages output from repository Markdown; no runtime dependencies."""
import argparse
import html
from html.parser import HTMLParser
import json
from pathlib import Path
import re
import shutil
from string import Template
import tomllib
from urllib.parse import quote, unquote, urlsplit

import markdown
from markdown.extensions.toc import slugify

REPO = Path(__file__).resolve().parent.parent
SITE = REPO / 'site'
GITHUB = 'https://github.com/JinShuo-Li/latch/blob/main/'


def excerpt(text, page):
    if 'section' not in page:
        return re.sub(r'\A# [^\n]+\n', '', text, count=1)
    # Match headings only outside fenced blocks (code comments can start with #).
    headings, fence = [], None
    for match in re.finditer(r'^.*$', text, re.M):
        line = match.group()
        marker = re.match(r'^\s*(`{3,}|~{3,})', line)
        if marker:
            if fence is None:
                fence = marker[1][0]
            elif marker[1][0] == fence:
                fence = None
        elif fence is None:
            heading = re.match(r'^(#{1,6}) (.+)$', line)
            if heading:
                headings.append((match.start(), len(heading[1]), heading[2]))
    start = next(i for i, h in enumerate(headings) if h[2] == page['section'])
    last = next(i for i in range(start, len(headings)) if headings[i][2] == page.get('through', page['section']))
    end = next((h[0] for h in headings[last + 1:] if h[1] <= headings[start][1]), len(text))
    return text[headings[start][0]:end]


class PlainText(HTMLParser):
    def __init__(self):
        super().__init__()
        self.parts = []

    def handle_data(self, data):
        self.parts.append(data)


def plain(value):
    parser = PlainText()
    parser.feed(value)
    return re.sub(r'\s+', ' ', ' '.join(parser.parts)).strip()


def build(output):
    # Refuse outputs that could overwrite tracked source or a repository parent.
    output = output.resolve()
    if output == REPO or REPO.is_relative_to(output) or output == SITE or output.is_relative_to(SITE):
        raise ValueError('Output must be a separate generated directory, e.g. target/site-preview')
    output.mkdir(parents=True, exist_ok=True)
    groups = json.loads((SITE / 'navigation.json').read_text())
    pages = [dict(page, group=group['title']) for group in groups for page in group['pages']]
    version = tomllib.loads((REPO / 'Cargo.toml').read_text())['workspace']['package']['version']
    template = Template((SITE / 'templates/page.html').read_text())
    for name in ('styles.css', 'app.js', 'favicon.svg'):
        shutil.copyfile(SITE / name, output / name)
    for name in ('assets', 'captures'):
        shutil.copytree(SITE / name, output / name, dirs_exist_ok=True)
    for name in ('install.sh', 'install.ps1'):
        shutil.copyfile(REPO / 'scripts' / name, output / name)
    (output / '.nojekyll').touch()
    sources = {p['source']: p for p in pages if 'section' not in p}
    sections = {(p['source'], slugify(p['section'], '-')): p for p in pages if 'section' in p}

    def frame(title, body, root, canonical, description, home=False):
        return template.substitute(title=html.escape(title), body=body, root=root, canonical=canonical,
                                   description=html.escape(description, quote=True), version=version,
                                   home_current='aria-current="page"' if home else '',
                                   docs_current='' if home else 'aria-current="true"')

    def rewrite_url(value, page):
        url = urlsplit(html.unescape(value))
        if url.scheme or url.netloc or not url.path:
            return html.unescape(value)
        path = (REPO / page['source']).parent / unquote(url.path)
        path = path.resolve()
        if not path.is_relative_to(REPO) or '.references' in path.parts:
            raise ValueError(f'Forbidden document reference: {value}')
        relative = path.relative_to(REPO).as_posix()
        fragment = unquote(url.fragment)
        target = sections.get((relative, fragment)) or sources.get(relative)
        if target:
            return f'../{target["slug"]}/' + (f'#{quote(fragment)}' if fragment else '')
        if path.suffix.lower() in ('.svg', '.png', '.jpg', '.webp') and path.is_file():
            asset = output / 'assets' / 'repo' / relative
            asset.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, asset)
            return '../../assets/repo/' + quote(relative)
        # Source files and unpublished research documents stay browsable on GitHub.
        if not path.exists():
            raise ValueError(f'Missing repository reference in {page["source"]}: {value}')
        return GITHUB + quote(relative) + (f'#{quote(fragment)}' if fragment else '')

    search = []
    for index, page in enumerate(pages):
        text = (REPO / page['source']).read_text()
        if page['source'].endswith('.toml'):
            text = 'The annotated example configuration is maintained in `config.example.toml`.\n\n```toml\n' + text + '\n```'
        else:
            text = excerpt(text, page)
        md = markdown.Markdown(extensions=['fenced_code', 'tables', 'toc', 'sane_lists'],
                               extension_configs={'toc': {'permalink': True, 'permalink_title': 'Link to this section'}})
        content = md.convert(text)
        content = re.sub(r'\b(href|src|srcset)="([^"]+)"',
                         lambda m: f'{m[1]}="{html.escape(rewrite_url(m[2], page), quote=True)}"', content)
        content = re.sub(r'<th(?=[ >])', '<th scope="col"', content)
        content = content.replace('<table>', '<div class="table-scroll" role="region" aria-label="Table" tabindex="0"><table>').replace('</table>', '</table></div>')
        content = content.replace('<pre><code class="language-mermaid">', '<p class="diagram-note">Architecture diagram source (Mermaid):</p><pre><code class="language-mermaid">')
        sidebar = '<details class="sidebar" open><summary>Documentation menu</summary><nav aria-label="Documentation">'
        for group in groups:
            sidebar += f'<div class="nav-group"><p>{html.escape(group["title"])}</p><ul>'
            for item in group['pages']:
                active = ' aria-current="page"' if page['slug'] == item['slug'] else ''
                sidebar += f'<li><a href="../{item["slug"]}/"{active}>{html.escape(item["title"])}</a></li>'
            sidebar += '</ul></div>'
        sidebar += '</nav></details>'
        pager = '<nav class="page-nav" aria-label="Documentation pages">'
        for offset, label in [(-1, 'Previous'), (1, 'Next')]:
            target_index = index + offset
            if 0 <= target_index < len(pages):
                item = pages[target_index]
                pager += f'<a href="../{item["slug"]}/"><small>{label}</small>{html.escape(item["title"])}</a>'
            else:
                pager += '<span></span>'
        pager += '</nav>'
        description = plain(content)[:180]
        body = f'''<div class="docs-layout">{sidebar}
<main id="main" class="doc-main"><article class="prose">
<header class="article-header"><p class="eyebrow">{html.escape(page['group'])}</p><h1>{html.escape(page['title'])}</h1></header>
{content}
<div class="source-note"><a href="{GITHUB}{quote(page['source'])}">Edit this page on GitHub ↗</a><span>Source: {html.escape(page['source'])}</span></div>
{pager}</article></main>
<aside class="page-outline" aria-label="On this page"><p>On this page</p>{md.toc}</aside></div>'''
        dest = output / 'docs' / page['slug']
        dest.mkdir(parents=True, exist_ok=True)
        (dest / 'index.html').write_text(frame(page['title'], body, '../..', f'docs/{page["slug"]}/', description))
        # Search sections have direct heading links and plain text; no remote service.
        chunks = re.split(r'(<h[1-6]\b[^>]*>.*?</h[1-6]>)', content, flags=re.S)
        heading, anchor = page['title'], ''
        for chunk in chunks:
            if re.match(r'<h[1-6]\b', chunk):
                heading = plain(re.sub(r'<a class="headerlink".*?</a>', '', chunk))
                match = re.search(r'\bid="([^"]+)"', chunk)
                anchor = '#' + match[1] if match else ''
            elif plain(chunk):
                search.append({'title': page['title'], 'section': heading, 'group': page['group'],
                               'url': f'docs/{page["slug"]}/{anchor}', 'text': plain(chunk)})
    home = Template((SITE / 'templates/home.html').read_text()).substitute(version=version)
    (output / 'index.html').write_text(frame('Terminal and browser coding agent', home, '.', '',
        'Latch is a Rust coding agent for Linux and Windows with mandatory sandboxing, durable sessions, and evidence-based validation.', home=True))
    (output / 'search-index.json').write_text(json.dumps(search, ensure_ascii=False, separators=(',', ':')))
    (output / 'docs/index.html').write_text('<!doctype html><html lang="en"><meta charset="utf-8"><title>Latch documentation</title><meta http-equiv="refresh" content="0;url=introduction/"><a href="introduction/">Latch documentation</a></html>')
    print(f'Built {len(pages)} documentation pages and {len(search)} search sections in {output}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=REPO / 'target/site-preview')
    build(parser.parse_args().output)
