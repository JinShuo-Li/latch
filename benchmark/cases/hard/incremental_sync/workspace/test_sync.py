import tempfile
import unittest
from pathlib import Path

from sync import sync_tree


class SyncTest(unittest.TestCase):
    def test_new_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); source = root / "src"; source.mkdir()
            (source / "a.txt").write_text("old")
            sync_tree(source, root / "dst")
            self.assertEqual((root / "dst/a.txt").read_text(), "old")

    def test_changed_same_size(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); source = root / "src"; source.mkdir()
            (source / "a.txt").write_text("old"); sync_tree(source, root / "dst")
            (source / "a.txt").write_text("new"); sync_tree(source, root / "dst")
            self.assertEqual((root / "dst/a.txt").read_text(), "new")


if __name__ == "__main__": unittest.main()
