"""Process complete JSONL jobs from an append-only spool."""

import json
import os
from pathlib import Path


def process_spool(spool_path, checkpoint_path, handler):
    spool_path, checkpoint_path = Path(spool_path), Path(checkpoint_path)
    offset = int(checkpoint_path.read_text()) if checkpoint_path.exists() else 0
    size = spool_path.stat().st_size
    if offset < 0 or offset > size:
        raise ValueError("checkpoint outside spool")
    count = 0
    with spool_path.open("rb") as spool:
        spool.seek(offset)
        while True:
            line = spool.readline()
            if not line or not line.endswith(b"\n"):
                break
            job = json.loads(line.decode("utf-8"))
            handler(job)
            offset = spool.tell()
            temporary = checkpoint_path.with_name(checkpoint_path.name + ".tmp")
            with temporary.open("w", encoding="ascii") as output:
                output.write(str(offset))
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, checkpoint_path)
            count += 1
    return count
