"""Incremental directory mirror."""

import json
import shutil
from pathlib import Path


def sync_tree(source, destination):
    source, destination = Path(source), Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    manifest = destination / ".manifest.json"
    previous = json.loads(manifest.read_text()) if manifest.exists() else {}
    current = {}
    for path in source.rglob("*"):
        if path.is_file():
            relative = path.relative_to(source).as_posix()
            current[relative] = str(path.stat().st_size)
            target = destination / relative
            if relative not in previous:
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, target)
    manifest.write_text(json.dumps(current))
    return current
