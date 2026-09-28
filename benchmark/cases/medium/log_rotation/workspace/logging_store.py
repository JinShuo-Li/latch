"""Size-bounded operational log."""

from pathlib import Path


def append_log(path, line, max_bytes, backups=2):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    encoded = (line + "\n").encode("utf-8")
    if path.exists() and path.stat().st_size + len(encoded) > max_bytes:
        archived = path.with_name(path.name + ".1")
        path.replace(archived)
    with path.open("ab") as stream:
        stream.write(encoded)
