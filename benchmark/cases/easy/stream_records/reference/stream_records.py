"""Parse newline-delimited JSON records from arbitrary byte chunks."""

import codecs
import json


def parse_records(chunks):
    records = []
    pending = ""
    decoder = codecs.getincrementaldecoder("utf-8")(errors="ignore")
    for chunk in chunks:
        pending += decoder.decode(chunk)
        lines = pending.split("\n")
        pending = lines.pop()
        for line in lines:
            if line.strip():
                records.append(json.loads(line))
    pending += decoder.decode(b"", final=True)
    if pending.strip():
        records.append(json.loads(pending))
    return records
