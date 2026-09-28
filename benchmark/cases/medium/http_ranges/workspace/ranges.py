"""Single byte-range download response."""


def serve_range(data, header):
    if not header:
        return 200, data, None
    if not header.startswith("bytes="):
        raise ValueError("invalid range")
    start, end = header[6:].split("-", 1)
    start = int(start or 0)
    end = int(end or len(data))
    body = data[start:end]
    return 206, body, f"bytes {start}-{end}/{len(data)}"
