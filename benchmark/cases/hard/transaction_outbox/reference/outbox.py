import json


def create_order(db, order_id, amount):
    if amount <= 0:
        raise ValueError("amount must be positive")
    db.execute("CREATE TABLE IF NOT EXISTS orders(id TEXT PRIMARY KEY, amount INTEGER)")
    db.execute("CREATE TABLE IF NOT EXISTS outbox(seq INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT)")
    db.commit()
    try:
        db.execute("BEGIN IMMEDIATE")
        db.execute("INSERT INTO orders VALUES(?,?)", (order_id, amount))
        db.execute("INSERT INTO outbox(payload) VALUES(?)", (json.dumps({"id": order_id, "amount": amount}),))
        db.commit()
    except BaseException:
        db.rollback()
        raise


def pending_events(db):
    return [json.loads(row[0]) for row in db.execute("SELECT payload FROM outbox ORDER BY seq")]
