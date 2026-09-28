import json
from pathlib import Path


class EventLog:
    def __init__(self, path):
        self.path = Path(path)

    def rebuild_index(self):
        if not self.path.exists():
            return []
        offsets = []
        with self.path.open("rb") as stream:
            while True:
                offset = stream.tell()
                line = stream.readline()
                if not line or not line.endswith(b"\n"):
                    break
                json.loads(line.decode("utf-8"))
                offsets.append(offset)
        return offsets

    def read_all(self):
        if not self.path.exists():
            return []
        with self.path.open("rb") as stream:
            return [json.loads(stream.readline().decode("utf-8")) for _ in self.rebuild_index()]

    def append(self, event):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        if self.path.exists():
            data = self.path.read_bytes()
            if data and not data.endswith(b"\n"):
                raise ValueError("partial tail")
        offset = self.path.stat().st_size if self.path.exists() else 0
        encoded = (json.dumps(event, ensure_ascii=False) + "\n").encode("utf-8")
        with self.path.open("ab") as stream:
            stream.write(encoded)
        return offset
