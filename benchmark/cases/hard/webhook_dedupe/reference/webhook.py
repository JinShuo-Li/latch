def process_event(db, event_id, payload, handler):
    db.execute("CREATE TABLE IF NOT EXISTS events(id TEXT PRIMARY KEY, done INTEGER)")
    db.commit()
    db.execute("BEGIN IMMEDIATE")
    try:
        existing = db.execute("SELECT done FROM events WHERE id=?", (event_id,)).fetchone()
        if existing and existing[0]:
            db.rollback()
            return False
        if not existing:
            db.execute("INSERT INTO events VALUES(?,0)", (event_id,))
        handler(payload)
        db.execute("UPDATE events SET done=1 WHERE id=?", (event_id,))
        db.commit()
        return True
    except BaseException:
        db.rollback()
        raise
