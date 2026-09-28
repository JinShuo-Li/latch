"""Run an acceptance manifest against a copied benchmark workspace."""

import json
import subprocess
import sys
from pathlib import Path


def main():
    case_dir = Path(sys.argv[1]).resolve()
    workspace = Path(sys.argv[2]).resolve()
    manifest = json.loads((case_dir / "acceptance.json").read_text(encoding="utf-8"))
    checks = []
    for item in manifest["checks"]:
        outcome = subprocess.run(
            [sys.executable, "-B", "-c", item["code"]], cwd=workspace,
            text=True, capture_output=True, timeout=15,
        )
        checks.append({
            "name": item["name"],
            "passed": outcome.returncode == 0,
            "detail": (outcome.stderr or outcome.stdout)[-500:],
        })
    print(json.dumps({"checks": checks}))


if __name__ == "__main__":
    main()
