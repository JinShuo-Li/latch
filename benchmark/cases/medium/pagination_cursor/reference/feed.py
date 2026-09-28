import base64
import json


def _decode(cursor):
    try:
        value = json.loads(base64.urlsafe_b64decode(cursor.encode()).decode())
        if not isinstance(value, list) or len(value) != 2:
            raise ValueError("invalid cursor")
        return tuple(value)
    except (ValueError, TypeError, UnicodeError) as error:
        raise ValueError("invalid cursor") from error


def page(records, cursor, limit):
    if not isinstance(limit, int) or limit <= 0:
        raise ValueError("limit must be positive")
    key = _decode(cursor) if cursor else None
    ordered = sorted(records, key=lambda row: (row["timestamp"], row["id"]))
    remaining = [row for row in ordered if key is None or
                 (row["timestamp"], row["id"]) > key]
    items = remaining[:limit]
    next_cursor = None
    if len(remaining) > limit:
        last = items[-1]
        next_cursor = base64.urlsafe_b64encode(
            json.dumps([last["timestamp"], last["id"]]).encode()).decode()
    return items, next_cursor
