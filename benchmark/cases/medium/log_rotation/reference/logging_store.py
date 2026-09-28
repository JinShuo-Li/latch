from pathlib import Path


def append_log(path, line, max_bytes, backups=2):
    if max_bytes <= 0 or backups < 0:
        raise ValueError("invalid rotation limits")
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    encoded = (line + "\n").encode("utf-8")
    if path.exists() and path.stat().st_size and path.stat().st_size + len(encoded) > max_bytes:
        if backups:
            oldest = path.with_name(path.name + f".{backups}")
            oldest.unlink(missing_ok=True)
            for number in range(backups - 1, 0, -1):
                older = path.with_name(path.name + f".{number}")
                if older.exists():
                    older.replace(path.with_name(path.name + f".{number + 1}"))
            path.replace(path.with_name(path.name + ".1"))
        else:
            path.unlink()
    with path.open("ab") as stream:
        stream.write(encoded)
