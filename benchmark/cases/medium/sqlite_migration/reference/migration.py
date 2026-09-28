def migrate(connection):
    table = connection.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='users'"
    ).fetchone()
    if not table:
        connection.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT)")
    else:
        columns = {row[1] for row in connection.execute("PRAGMA table_info(users)")}
        if not {"id", "name"}.issubset(columns):
            raise ValueError("unsupported users schema")
        if "email" not in columns:
            connection.execute("ALTER TABLE users ADD COLUMN email TEXT")
    connection.execute("PRAGMA user_version = 2")
    connection.commit()
