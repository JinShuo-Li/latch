"""Append-only JSONL event log."""

import json
from pathlib import Path


class EventLog:
    def __init__(self, path):
        self.path = Path(path)

    def append(self, event):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        offset = self.path.stat().st_size if self.path.exists() else 0
        with self.path.open("ab") as stream:
            stream.write((json.dumps(event, ensure_ascii=False) + "\n").encode("utf-8"))
        return offset

    def read_all(self):
        return [json.loads(line) for line in self.path.read_text(encoding="utf-8").splitlines()]

    def rebuild_index(self):
        return list(range(len(self.read_all())))
