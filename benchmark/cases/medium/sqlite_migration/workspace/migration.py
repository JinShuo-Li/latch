"""Address-book schema upgrade."""


def migrate(connection):
    connection.execute("DROP TABLE IF EXISTS users")
    connection.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT)")
    connection.execute("PRAGMA user_version = 2")
    connection.commit()
