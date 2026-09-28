#!/usr/bin/env python3
"""Check that each new case fails at baseline and has a passing reference."""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from run import check, discover


def main():
    failures = []
    for data, case_dir in discover().values():
        reference = case_dir / "reference"
        if not reference.is_dir():
            failures.append((data["id"], [], [], "missing reference fixture"))
            continue
        with tempfile.TemporaryDirectory(prefix="latch-case-verify-") as temporary:
            workspace = Path(temporary) / "workspace"
            shutil.copytree(case_dir / "workspace", workspace)
            before = check(case_dir, workspace)
            for source in reference.rglob("*"):
                if source.is_file():
                    destination = workspace / source.relative_to(reference)
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(source, destination)
            after = check(case_dir, workspace)
            visible = subprocess.run(
                [sys.executable, "-B", "-m", "unittest", "discover", "-s", str(workspace)],
                cwd=workspace, text=True, capture_output=True, timeout=30,
            )
            okay = any(not item["passed"] for item in before)
            okay &= all(item["passed"] for item in after)
            okay &= visible.returncode == 0
            print(f"{data['tier']:6} {data['id']:20} baseline "
                  f"{sum(item['passed'] for item in before)}/{len(before)}; "
                  f"reference {sum(item['passed'] for item in after)}/{len(after)}; "
                  f"visible {'ok' if visible.returncode == 0 else 'FAIL'}")
            if not okay:
                failures.append((data["id"], before, after, visible.stderr))
    for name, before, after, stderr in failures:
        print(f"\n{name}: baseline={before}\nreference={after}\nvisible={stderr}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
