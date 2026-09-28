"""Parse newline-delimited JSON events from arbitrary network byte chunks."""

import json


def parse_records(chunks):
    """Return JSON values from chunks; accept a final line without a newline."""
    records = []
    pending = ""
    for chunk in chunks:
        # A network read may end between two bytes of one UTF-8 character.
        pending += chunk.decode("utf-8", errors="ignore")
        lines = pending.split("\n")
        pending = lines.pop()
        for line in lines:
            if line.strip():
                records.append(json.loads(line))
    if pending.strip():
        records.append(json.loads(pending))
    return records
