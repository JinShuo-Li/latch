#!/usr/bin/env python3
"""Summarize a session's first run in a closed database; no provider calls."""

import argparse
import collections
import datetime
import hashlib
import json
from pathlib import Path
import sqlite3


def timestamp(value):
    return datetime.datetime.fromisoformat(value).timestamp()


def summarize(database, session):
    # Immutable mode avoids creating SQLite sidecars. It requires a closed,
    # checkpointed database, so refuse a remaining WAL rather than ignore it.
    if Path(str(database) + "-wal").exists():
        raise ValueError("Close and checkpoint the database before summarizing it")
    uri = database.resolve().as_uri() + "?mode=ro&immutable=1"
    connection = sqlite3.connect(uri, uri=True)
    try:
        rows = [
            (sequence, time, kind, json.loads(payload)["data"])
            for sequence, time, kind, payload in connection.execute(
                "select sequence,timestamp,kind,payload from events "
                "where session_id=? order by sequence",
                (session,),
            )
        ]
    finally:
        connection.close()
    start = next((row for row in rows if row[2] == "run_started"), None)
    if start is None:
        raise ValueError("No run_started event for the selected session")
    end = next((row for row in rows if row[2] == "run_completed"), None)
    rows = [
        row for row in rows if row[0] >= start[0] and (end is None or row[0] <= end[0])
    ]
    calls = [(s, t, d["call"]) for s, t, k, d in rows if k == "tool_requested"]
    counts = collections.Counter(k for s, t, k, d in rows)
    results = {
        d["result"]["call_id"]: d["result"]
        for s, t, k, d in rows
        if k in ("tool_completed", "tool_failed")
    }
    reads = collections.defaultdict(list)
    for sequence, time, call in calls:
        if call["name"] == "read_file":
            reads[json.dumps(call["arguments"], sort_keys=True)].append(call)
    duplicates = 0
    for group in reads.values():
        hashes = collections.Counter(
            hashlib.sha256(results[call["id"]]["output"].encode()).hexdigest()
            for call in group
            if call["id"] in results
        )
        duplicates += sum(count - 1 for count in hashes.values())
    first_edit = next((row for row in rows if row[2] == "file_changed"), None)
    usage = collections.Counter()
    request_start = None
    intervals = []
    for sequence, time, kind, data in rows:
        if kind == "model_usage":
            for field, value in data["usage"].items():
                if isinstance(value, int):
                    usage[field] += value
        elif kind == "model_request_started":
            request_start = timestamp(time)
        elif kind == "model_request_finished" and request_start is not None:
            intervals.append(timestamp(time) - request_start)
            request_start = None
    return {
        "session_id": session,
        "duration_seconds": round(timestamp(end[1]) - timestamp(start[1]), 3)
        if end
        else None,
        "outcome": end[3]["outcome"] if end else None,
        "first_edit_seconds": round(timestamp(first_edit[1]) - timestamp(start[1]), 3)
        if first_edit
        else None,
        "tool_calls": counts["tool_requested"],
        "model_requests": counts["model_request_started"],
        "epochs": counts["context_epoch_started"],
        "reads": sum(call["name"] == "read_file" for s, t, call in calls),
        "exact_duplicate_reads_same_output": duplicates,
        "tools": dict(collections.Counter(call["name"] for s, t, call in calls)),
        "read_paths": dict(
            collections.Counter(
                call["arguments"].get("path")
                for s, t, call in calls
                if call["name"] == "read_file"
            )
        ),
        "usage": dict(usage),
        "permission_requests": counts["permission_requested"],
        "started_processes": counts["process_started"],
        "exited_processes": counts["process_exited"],
        "model_request_interval_sum_seconds": round(sum(intervals), 3),
        "model_request_interval_max_seconds": round(max(intervals, default=0), 3),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("database", type=Path)
    parser.add_argument("session")
    args = parser.parse_args()
    try:
        result = summarize(args.database, args.session)
    except (ValueError, sqlite3.Error) as error:
        parser.error(str(error))
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
