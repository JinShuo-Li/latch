import unittest

from options import parse_args


class OptionsTest(unittest.TestCase):
    def test_basic(self):
        self.assertEqual(parse_args(["--label", "nightly", "a.zip"]),
                         {"label": "nightly", "quiet": False, "files": ["a.zip"]})

    def test_empty_label_and_end_marker(self):
        self.assertEqual(parse_args(["--label", "", "--", "--quiet"]),
                         {"label": "", "quiet": False, "files": ["--quiet"]})


if __name__ == "__main__":
    unittest.main()
