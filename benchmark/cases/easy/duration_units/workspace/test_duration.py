import unittest

from duration import parse_duration


class DurationTest(unittest.TestCase):
    def test_seconds(self):
        self.assertEqual(parse_duration("2s"), 2000)

    def test_milliseconds(self):
        self.assertEqual(parse_duration("250ms"), 250)


if __name__ == "__main__":
    unittest.main()
