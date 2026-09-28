"""Evaluator-only checks. This file is never copied into the agent workspace."""

import json
import subprocess
import sys
from pathlib import Path


def check(name, expression, expected, cwd):
    code = "from config_layers import resolve_config; import json; print(json.dumps(" + expression + ", sort_keys=True))"
    result = subprocess.run([sys.executable, "-c", code], cwd=cwd, text=True,
                            capture_output=True, timeout=10)
    try:
        actual = json.loads(result.stdout) if result.returncode == 0 else None
    except json.JSONDecodeError:
        actual = None
    return {"name": name, "passed": actual == expected,
            "detail": result.stderr[-500:] or repr(actual)}


root = Path(sys.argv[1]).resolve()
base = "{'service': {'endpoint': 'default', 'timeout': 30, 'retries': 2}, 'output': {'format': 'text', 'color': True}}"
checks = [
    check("nested_merge", f"resolve_config({base}, {{'service': {{'timeout': 12}}, 'output': {{'format': 'json'}}}}, {{}}, {{}})",
          {"service": {"endpoint": "default", "timeout": 12, "retries": 2},
           "output": {"format": "json", "color": True}}, root),
    check("explicit_empty_after_environment", f"resolve_config({base}, {{}}, {{'APP_ENDPOINT': 'env'}}, {{'endpoint': ''}})['service']['endpoint']", "", root),
    check("zero_and_false_are_explicit", f"resolve_config({base}, {{}}, {{}}, {{'timeout': 0, 'quiet': False}})",
          {"service": {"endpoint": "default", "timeout": 0, "retries": 2},
           "output": {"format": "text", "color": True}, "quiet": False}, root),
    check("inputs_unchanged", f"(lambda d, p: (resolve_config(d, p, {{'APP_TIMEOUT': '7'}}, {{'endpoint': 'flag'}}), d, p)[1:])({base}, {{'service': {{'timeout': 9}}}})",
          [{"service": {"endpoint": "default", "timeout": 30, "retries": 2},
            "output": {"format": "text", "color": True}}, {"service": {"timeout": 9}}], root),
]
print(json.dumps({"checks": checks}))
