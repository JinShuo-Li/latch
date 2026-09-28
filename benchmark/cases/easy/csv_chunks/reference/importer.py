"""Reference behavior for benchmark author verification."""

import csv
import io


def parse_chunks(chunks):
    return list(csv.reader(io.StringIO("".join(chunks), newline="")))
