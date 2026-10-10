#!/usr/bin/env python3
"""Check every generated local link, fragment, asset and search result."""
from html.parser import HTMLParser
import json
from pathlib import Path
import sys
from urllib.parse import unquote, urlsplit


class Page(HTMLParser):
    def __init__(self, path):
        super().__init__()
        self.ids, self.refs, self.errors = set(), [], []
        self.feed(path.read_text())

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if 'id' in attrs:
            if attrs['id'] in self.ids:
                self.errors.append('duplicate id: ' + attrs['id'])
            self.ids.add(attrs['id'])
        for name in ('href', 'src', 'srcset'):
            if name in attrs:
                self.refs.append(attrs[name])
        if tag == 'img' and not attrs.get('alt'):
            self.errors.append('image without alt text')
        if tag == 'html' and attrs.get('lang') != 'en':
            self.errors.append('missing English document language')


def check(root):
    root = root.resolve()
    pages = {path: Page(path) for path in root.rglob('*.html')}
    errors = []
    count = 0
    def reference(source, value):
        nonlocal count
        url = urlsplit(value)
        if url.scheme or url.netloc:
            return
        count += 1
        target = (source.parent / unquote(url.path)).resolve() if url.path else source
        if target.is_dir():
            target /= 'index.html'
        if not target.is_relative_to(root) or not target.is_file():
            errors.append(f'{source.relative_to(root)}: missing local target {value}')
        elif url.fragment and target in pages and unquote(url.fragment) not in pages[target].ids:
            errors.append(f'{source.relative_to(root)}: missing fragment {value}')
    for path, page in pages.items():
        errors.extend(f'{path.relative_to(root)}: {error}' for error in page.errors)
        for value in page.refs:
            reference(path, value)
    index = json.loads((root / 'search-index.json').read_text())
    for entry in index:
        reference(root / 'index.html', entry['url'])
        if not entry['text']:
            errors.append('empty search section: ' + entry['url'])
    if (root / 'install.sh').read_bytes() != (Path(__file__).resolve().parent / 'install.sh').read_bytes():
        errors.append('Linux installer differs from authoritative source')
    if (root / 'install.ps1').read_bytes() != (Path(__file__).resolve().parent / 'install.ps1').read_bytes():
        errors.append('Windows installer differs from authoritative source')
    if errors:
        raise SystemExit('\n'.join(errors))
    print(f'Checked {len(pages)} HTML pages, {count} local links/assets/fragments, and {len(index)} search sections.')


if __name__ == '__main__':
    check(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/site-preview'))
