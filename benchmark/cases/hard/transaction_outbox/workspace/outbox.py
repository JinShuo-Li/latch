"""Order creation and durable event outbox."""

import json


def create_order(db, order_id, amount):
    db.execute("CREATE TABLE IF NOT EXISTS orders(id TEXT PRIMARY KEY, amount INTEGER)")
    db.execute("CREATE TABLE IF NOT EXISTS outbox(seq INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT)")
    db.execute("INSERT INTO orders VALUES(?,?)", (order_id, amount))
    db.commit()
    db.execute("INSERT INTO outbox(payload) VALUES(?)", (json.dumps({"id": order_id, "amount": amount}),))
    db.commit()


def pending_events(db):
    return [json.loads(row[0]) for row in db.execute("SELECT payload FROM outbox ORDER BY seq")]
