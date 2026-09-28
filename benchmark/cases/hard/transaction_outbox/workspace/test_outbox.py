import sqlite3
import unittest

from outbox import create_order, pending_events


class OutboxTest(unittest.TestCase):
    def test_order_and_event(self):
        db = sqlite3.connect(":memory:")
        create_order(db, "o1", 10)
        self.assertEqual(db.execute("SELECT amount FROM orders WHERE id='o1'").fetchone(), (10,))
        self.assertEqual(pending_events(db), [{"id": "o1", "amount": 10}])

    def test_event_insert_failure_rolls_back_order(self):
        db = sqlite3.connect(":memory:")
        create_order(db, "first", 1)
        db.execute("CREATE TRIGGER reject_event BEFORE INSERT ON outbox BEGIN SELECT RAISE(ABORT,'down'); END")
        with self.assertRaises(sqlite3.DatabaseError):
            create_order(db, "second", 2)
        self.assertIsNone(db.execute("SELECT id FROM orders WHERE id='second'").fetchone())


if __name__ == "__main__": unittest.main()
