"""Webhook idempotency backed by SQLite."""


def process_event(db, event_id, payload, handler):
    db.execute("CREATE TABLE IF NOT EXISTS events(id TEXT PRIMARY KEY, done INTEGER)")
    existing = db.execute("SELECT done FROM events WHERE id=?", (event_id,)).fetchone()
    if existing:
        return False
    db.execute("INSERT INTO events VALUES(?,1)", (event_id,))
    db.commit()
    handler(payload)
    return True
