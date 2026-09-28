import unittest

from ranges import serve_range


class RangesTest(unittest.TestCase):
    def test_full(self):
        self.assertEqual(serve_range(b"abcdef", None), (200, b"abcdef", None))

    def test_inclusive_end(self):
        self.assertEqual(serve_range(b"abcdef", "bytes=1-3"),
                         (206, b"bcd", "bytes 1-3/6"))


if __name__ == "__main__":
    unittest.main()
