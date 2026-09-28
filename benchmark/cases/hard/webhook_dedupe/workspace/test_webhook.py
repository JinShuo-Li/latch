import sqlite3
import unittest

from webhook import process_event


class WebhookTest(unittest.TestCase):
    def test_duplicate(self):
        db = sqlite3.connect(":memory:"); seen = []
        self.assertTrue(process_event(db, "id", 1, seen.append))
        self.assertFalse(process_event(db, "id", 1, seen.append))
        self.assertEqual(seen, [1])

    def test_failed_handler_retries(self):
        db = sqlite3.connect(":memory:")
        with self.assertRaises(RuntimeError):
            process_event(db, "id", 1, lambda _: (_ for _ in ()).throw(RuntimeError()))
        self.assertTrue(process_event(db, "id", 1, lambda _: None))


if __name__ == "__main__": unittest.main()
