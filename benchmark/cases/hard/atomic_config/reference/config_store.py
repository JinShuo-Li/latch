import json
import os
import tempfile
from pathlib import Path


def _validated(raw):
    result = json.loads(raw)
    if not isinstance(result, dict):
        raise ValueError("config must be an object")
    return result


def _replace_bytes(path, raw):
    fd, temporary = tempfile.mkstemp(dir=path.parent, prefix=".config-")
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def write_config(path, data):
    if not isinstance(data, dict):
        raise ValueError("config must be an object")
    raw = json.dumps(data).encode("utf-8")
    _validated(raw)
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        previous = path.read_bytes()
        _validated(previous)
        _replace_bytes(path.with_name(path.name + ".bak"), previous)
    _replace_bytes(path, raw)


def read_config(path):
    path = Path(path)
    try:
        return _validated(path.read_bytes())
    except (ValueError, OSError):
        return _validated(path.with_name(path.name + ".bak").read_bytes())
