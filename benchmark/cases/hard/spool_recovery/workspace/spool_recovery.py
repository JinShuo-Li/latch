"""Process JSONL jobs from an append-only spool with a durable byte checkpoint."""

import json
from pathlib import Path


def _save_offset(path, offset):
    Path(path).write_text(str(offset), encoding="ascii")


def process_spool(spool_path, checkpoint_path, handler):
    """Call handler for each complete unprocessed record; return job count."""
    spool_path = Path(spool_path)
    checkpoint_path = Path(checkpoint_path)
    offset = int(checkpoint_path.read_text(encoding="ascii")) if checkpoint_path.exists() else 0
    count = 0
    with spool_path.open("rb") as spool:
        spool.seek(offset)
        for raw_line in spool:
            job = json.loads(raw_line.decode("utf-8"))
            offset = spool.tell()
            _save_offset(checkpoint_path, offset)
            handler(job)
            count += 1
    return count
