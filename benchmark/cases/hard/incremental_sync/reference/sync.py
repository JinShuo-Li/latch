import hashlib
import json
import shutil
from pathlib import Path


def sync_tree(source, destination):
    source, destination = Path(source), Path(destination)
    files = {}
    for path in source.rglob("*"):
        if path.is_symlink():
            raise ValueError("symlink source")
        if path.is_file():
            files[path.relative_to(source).as_posix()] = (path, hashlib.sha256(path.read_bytes()).hexdigest())
    destination.mkdir(parents=True, exist_ok=True)
    manifest = destination / ".manifest.json"
    previous = json.loads(manifest.read_text()) if manifest.exists() else {}
    current = {name: digest for name, (_, digest) in files.items()}
    for name, (path, digest) in files.items():
        target = destination / name
        if previous.get(name) != digest or not target.exists():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
    for name in previous.keys() - current.keys():
        (destination / name).unlink(missing_ok=True)
    manifest.write_text(json.dumps(current, sort_keys=True))
    return current
