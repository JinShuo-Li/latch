import unittest

from feed import page


ROWS = [{"timestamp": 1, "id": "a"}, {"timestamp": 1, "id": "b"},
        {"timestamp": 2, "id": "c"}, {"timestamp": 3, "id": "d"}]


class FeedTest(unittest.TestCase):
    def test_two_pages(self):
        first, cursor = page(ROWS, None, 2)
        second, end = page(ROWS, cursor, 2)
        self.assertEqual(first + second, ROWS)
        self.assertIsNone(end)

    def test_delete_earlier_record(self):
        first, cursor = page(ROWS, None, 2)
        second, _ = page(ROWS[1:], cursor, 2)
        self.assertEqual([row["id"] for row in first + second], ["a", "b", "c", "d"])


if __name__ == "__main__":
    unittest.main()
