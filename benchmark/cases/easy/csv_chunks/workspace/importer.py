"""CSV import for chunks read from a socket."""

import csv


def parse_chunks(chunks):
    rows = []
    for chunk in chunks:
        for line in chunk.splitlines():
            if line:
                rows.extend(csv.reader([line]))
    return rows
