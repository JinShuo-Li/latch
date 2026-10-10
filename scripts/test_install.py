#!/usr/bin/env python3
"""Offline installer regression tests: real archives, mocked HTTPS downloads."""
import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

INSTALLER = Path(__file__).resolve().with_name('install.sh')
ASSET = 'latch-x86_64-unknown-linux-gnu.tar.gz'
REPO = 'https://github.com/JinShuo-Li/latch'
BINARY = b'#!/bin/sh\necho latch-fixture\n'


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='latch-installer-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / 'mock-bin'
        self.bin.mkdir()
        self.downloads = self.root / 'downloads'
        self.downloads.mkdir()
        self.work = self.root / 'tmp'
        self.work.mkdir()
        self.destination = self.root / 'user bin'
        self.destination.mkdir()
        self.existing = self.destination / 'latch'
        self.existing.write_bytes(b'existing installation')
        self.archive()
        self.mock('curl', '''#!/usr/bin/env python3
import os, pathlib, shutil, sys
args = sys.argv[1:]
url = next(arg for arg in args if arg.startswith('https://'))
root = pathlib.Path(os.environ['LATCH_FIXTURE'])
with (root / 'requests').open('a') as log: log.write(url + '\\n')
repo = 'https://github.com/JinShuo-Li/latch'
if url == repo + '/releases/latest':
    print(repo + '/releases/tag/v9.8.7', end='')
else:
    assert url.startswith(repo + '/releases/download/v9.8.7/'), url
    source = root / url.rsplit('/', 1)[1]
    if not source.exists(): sys.exit(22)
    shutil.copyfile(source, args[args.index('-o') + 1])
''')
        self.mock('uname', '#!/bin/sh\nif [ "$1" = -s ]; then echo Linux; else echo x86_64; fi\n')
        self.env = os.environ | {
            'PATH': str(self.bin) + os.pathsep + os.environ['PATH'],
            'LATCH_FIXTURE': str(self.downloads),
            'LATCH_INSTALL_DIR': str(self.destination),
            'LATCH_VERSION': 'latest',
            'TMPDIR': str(self.work),
        }

    def mock(self, name, text):
        path = self.bin / name
        path.write_text(text)
        path.chmod(0o755)

    def archive(self, name='latch', symlink=False):
        with tarfile.open(self.downloads / ASSET, 'w:gz') as archive:
            entry = tarfile.TarInfo(name)
            if symlink:
                entry.type = tarfile.SYMTYPE
                entry.linkname = '/etc/passwd'
                archive.addfile(entry)
            else:
                entry.size = len(BINARY)
                archive.addfile(entry, io.BytesIO(BINARY))
        digest = hashlib.sha256((self.downloads / ASSET).read_bytes()).hexdigest()
        (self.downloads / 'SHA256SUMS').write_text(f'{digest}  {ASSET}\n')

    def run_installer(self, *args, succeeds=True, pipe=False):
        result = subprocess.run(
            ['bash', '-s', '--', *args] if pipe else ['bash', str(INSTALLER), *args],
            input=INSTALLER.read_text() if pipe else None,
            env=self.env, text=True, capture_output=True, timeout=10,
        )
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode == 0, succeeds, output)
        self.assertEqual(list(self.work.iterdir()), [], 'Downloaded files leaked')
        self.assertEqual(list(self.destination.glob('.latch-install.*')), [], 'Staged binary leaked')
        if succeeds:
            if '--help' not in args:
                self.assertEqual(self.existing.read_bytes(), BINARY)
                self.assertTrue(os.access(self.existing, os.X_OK))
                self.assertIn('SHA256 verified', output)
                self.assertIn('latch doctor', output)
        else:
            self.assertEqual(self.existing.read_bytes(), b'existing installation')
        return output

    def test_latest_piped_install_and_atomic_upgrade(self):
        self.run_installer(pipe=True)
        self.assertEqual((self.downloads / 'requests').read_text().splitlines(), [
            REPO + '/releases/latest',
            REPO + '/releases/download/v9.8.7/SHA256SUMS',
            REPO + '/releases/download/v9.8.7/' + ASSET,
        ])

    def test_legacy_versioned_archive(self):
        legacy = 'latch-v9.8.7-x86_64-unknown-linux-gnu'
        self.archive(name=legacy + '/latch')
        (self.downloads / ASSET).rename(self.downloads / (legacy + '.tar.gz'))
        manifest = self.downloads / 'SHA256SUMS'
        manifest.write_text(manifest.read_text().replace(ASSET, legacy + '.tar.gz'))
        self.run_installer()

    def test_pinned_version_normalizes_v_prefix(self):
        self.run_installer('--version', '9.8.7')
        self.assertNotIn('/latest', (self.downloads / 'requests').read_text())

    def test_environment_version(self):
        self.env['LATCH_VERSION'] = 'v9.8.7'
        self.run_installer()

    def test_checksum_mismatch_preserves_existing_binary(self):
        (self.downloads / ASSET).write_bytes(b'corrupted download')
        self.assertIn('SHA256 mismatch', self.run_installer(succeeds=False))

    def test_missing_checksum_fails_closed(self):
        (self.downloads / 'SHA256SUMS').unlink()
        self.assertIn('Cannot download', self.run_installer(succeeds=False))

    def test_unlisted_asset_fails_closed(self):
        (self.downloads / 'SHA256SUMS').write_text('0' * 64 + '  other.tar.gz\n')
        self.assertIn('invalid checksum', self.run_installer(succeeds=False))

    def test_duplicate_checksum_fails_closed(self):
        manifest = self.downloads / 'SHA256SUMS'
        manifest.write_text(manifest.read_text() * 2)
        self.assertIn('invalid checksum', self.run_installer(succeeds=False))

    def test_missing_binary(self):
        self.archive(name='other')
        self.assertIn('does not contain latch', self.run_installer(succeeds=False))

    def test_symlink_binary_rejected(self):
        self.archive(symlink=True)
        self.assertIn('regular latch binary', self.run_installer(succeeds=False))

    def test_missing_release(self):
        (self.downloads / ASSET).unlink()
        self.assertIn('Cannot download', self.run_installer(succeeds=False))

    def test_unsupported_architecture_and_os(self):
        for os_name, arch in [('Linux', 'armv7l'), ('Darwin', 'x86_64')]:
            self.mock('uname', f'#!/bin/sh\nif [ "$1" = -s ]; then echo {os_name}; else echo {arch}; fi\n')
            self.assertIn('Unsupported', self.run_installer(succeeds=False))
            self.assertFalse((self.downloads / 'requests').exists())

    def test_bad_options_do_not_download(self):
        for args in [('--version',), ('--install-dir',), ('--unknown',), ('--version', '../bad')]:
            self.run_installer(*args, succeeds=False)
        self.assertFalse((self.downloads / 'requests').exists())

    def test_help(self):
        self.assertIn('Usage:', self.run_installer('--help'))
        self.assertFalse((self.downloads / 'requests').exists())


if __name__ == '__main__':
    unittest.main()
