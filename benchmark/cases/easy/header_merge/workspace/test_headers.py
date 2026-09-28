import unittest

from headers import normalize_headers


class HeadersTest(unittest.TestCase):
    def test_single_header(self):
        self.assertEqual(normalize_headers([("x-id", "7")]), {"x-id": "7"})

    def test_duplicates(self):
        self.assertEqual(normalize_headers([("Accept", "text/plain"), ("accept", "application/json")]),
                         {"accept": "text/plain, application/json"})


if __name__ == "__main__":
    unittest.main()
