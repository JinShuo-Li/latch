import tempfile
import unittest
from pathlib import Path

from config_store import read_config, write_config


class ConfigStoreTest(unittest.TestCase):
    def test_round_trip(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "settings.json"
            write_config(path, {"port": 8080})
            self.assertEqual(read_config(path), {"port": 8080})

    def test_invalid_write_keeps_previous(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "settings.json"
            write_config(path, {"port": 8080})
            with self.assertRaises(TypeError):
                write_config(path, {"bad": {1}})
            self.assertEqual(read_config(path), {"port": 8080})


if __name__ == "__main__": unittest.main()
