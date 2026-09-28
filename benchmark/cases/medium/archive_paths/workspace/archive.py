"""ZIP document import."""

import zipfile
from pathlib import Path


def extract_archive(archive, destination):
    destination = Path(destination)
    extracted = []
    with zipfile.ZipFile(archive) as source:
        for member in source.infolist():
            if member.is_dir():
                continue
            target = destination / member.filename
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(source.read(member))
            extracted.append(member.filename)
    return extracted
