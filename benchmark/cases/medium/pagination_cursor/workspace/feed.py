"""Ascending activity feed pagination."""

import base64


def page(records, cursor, limit):
    ordered = sorted(records, key=lambda row: (row["timestamp"], row["id"]))
    start = int(base64.urlsafe_b64decode(cursor.encode()).decode()) if cursor else 0
    items = ordered[start:start + limit]
    next_cursor = None
    if start + limit < len(ordered):
        next_cursor = base64.urlsafe_b64encode(str(start + limit).encode()).decode()
    return items, next_cursor
