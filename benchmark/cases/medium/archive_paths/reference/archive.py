import stat
import zipfile
from pathlib import Path, PurePosixPath


def extract_archive(archive, destination):
    destination = Path(destination)
    with zipfile.ZipFile(archive) as source:
        files = []
        for member in source.infolist():
            path = PurePosixPath(member.filename)
            if (path.is_absolute() or ".." in path.parts or
                    not path.parts or path.parts[0] in ("", ".") or
                    stat.S_ISLNK(member.external_attr >> 16)):
                raise ValueError("unsafe archive entry")
            if not member.is_dir():
                files.append((member, path))
        extracted = []
        for member, path in files:
            target = destination.joinpath(*path.parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(source.read(member))
            extracted.append(path.as_posix())
        return extracted
