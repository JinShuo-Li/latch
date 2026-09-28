import sqlite3
import unittest

from migration import migrate


class MigrationTest(unittest.TestCase):
    def test_new_database(self):
        db = sqlite3.connect(":memory:")
        migrate(db)
        db.execute("INSERT INTO users(id,name,email) VALUES(1,'Ada','a@example.test')")
        self.assertEqual(db.execute("SELECT name FROM users").fetchone(), ("Ada",))

    def test_old_rows_survive(self):
        db = sqlite3.connect(":memory:")
        db.execute("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        db.execute("INSERT INTO users VALUES(1,'Ada')")
        migrate(db)
        self.assertEqual(db.execute("SELECT id,name,email FROM users").fetchall(),
                         [(1, "Ada", None)])


if __name__ == "__main__":
    unittest.main()
