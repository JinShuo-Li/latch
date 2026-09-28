import tempfile
import unittest
import zipfile
from pathlib import Path

from archive import extract_archive


class ArchiveTest(unittest.TestCase):
    def test_nested_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "input.zip"
            with zipfile.ZipFile(archive, "w") as output:
                output.writestr("docs/readme.txt", "hello")
            self.assertEqual(extract_archive(archive, root / "out"), ["docs/readme.txt"])
            self.assertEqual((root / "out/docs/readme.txt").read_text(), "hello")

    def test_traversal_does_not_write(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "input.zip"
            with zipfile.ZipFile(archive, "w") as output:
                output.writestr("../escaped.txt", "bad")
            with self.assertRaises(ValueError):
                extract_archive(archive, root / "out")
            self.assertFalse((root / "escaped.txt").exists())


if __name__ == "__main__":
    unittest.main()
