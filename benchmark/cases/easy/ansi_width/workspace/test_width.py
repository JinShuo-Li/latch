import unittest

from width import display_width


class WidthTest(unittest.TestCase):
    def test_plain_ascii(self):
        self.assertEqual(display_width("ready"), 5)

    def test_color_and_wide(self):
        self.assertEqual(display_width("\x1b[32m中\x1b[0m"), 2)


if __name__ == "__main__":
    unittest.main()
