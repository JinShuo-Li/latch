import re


def serve_range(data, header):
    if not header:
        return 200, data, None
    match = re.fullmatch(r"bytes=(\d*)-(\d*)", header)
    if not match or (not match.group(1) and not match.group(2)):
        raise ValueError("invalid range")
    size = len(data)
    if not match.group(1):
        count = int(match.group(2))
        if count <= 0:
            raise ValueError("empty suffix")
        start, end = max(0, size - count), size - 1
    else:
        start = int(match.group(1))
        end = int(match.group(2)) if match.group(2) else size - 1
        end = min(end, size - 1)
    if start >= size or end < start:
        return 416, b"", f"bytes */{size}"
    return 206, data[start:end + 1], f"bytes {start}-{end}/{size}"
