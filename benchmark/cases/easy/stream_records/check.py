"""Evaluator-only checks. This file is never copied into the agent workspace."""

import json
import subprocess
import sys
from pathlib import Path


def check(name, source, expectation, cwd):
    code = "from stream_records import parse_records; import json; print(json.dumps(parse_records(" + source + "), ensure_ascii=False))"
    result = subprocess.run(
        [sys.executable, "-c", code], cwd=cwd, text=True, capture_output=True, timeout=10
    )
    try:
        actual = json.loads(result.stdout) if result.returncode == 0 else None
    except json.JSONDecodeError:
        actual = None
    return {"name": name, "passed": actual == expectation, "detail": result.stderr[-500:] or repr(actual)}


root = Path(sys.argv[1]).resolve()
checks = [
    check("single_utf8_split", "[b'{\"name\":\"\\xe4', b'\\xb8\\xad\"}\\n']", [{"name": "中"}], root),
    check("every_utf8_boundary", "[bytes([b]) for b in '{\"name\":\"雪\"}\\n'.encode()]", [{"name": "雪"}], root),
    check("several_records_and_final_line", "[b'{\"id\":1}\\r\\n{\"id\":2}\\n{\"id\":3}']", [{"id": 1}, {"id": 2}, {"id": 3}], root),
    check("empty_chunks", "[b'', b'{\"id\":4}', b'']", [{"id": 4}], root),
]
print(json.dumps({"checks": checks}))
