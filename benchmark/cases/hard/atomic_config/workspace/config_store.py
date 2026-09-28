"""Daemon JSON configuration with backup."""

import json
from pathlib import Path


def write_config(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        path.with_name(path.name + ".bak").write_bytes(path.read_bytes())
    path.write_text(json.dumps(data), encoding="utf-8")


def read_config(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))
