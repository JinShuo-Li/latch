import tempfile
import unittest
from pathlib import Path

from logging_store import append_log


class RotationTest(unittest.TestCase):
    def test_small_append(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "app.log"
            append_log(path, "one", 100)
            append_log(path, "two", 100)
            self.assertEqual(path.read_text(), "one\ntwo\n")

    def test_archive_order(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "app.log"
            for text in ("one", "two", "three"):
                append_log(path, text, 5, backups=2)
            self.assertEqual((Path(directory) / "app.log.1").read_text(), "two\n")
            self.assertEqual((Path(directory) / "app.log.2").read_text(), "one\n")


if __name__ == "__main__":
    unittest.main()
