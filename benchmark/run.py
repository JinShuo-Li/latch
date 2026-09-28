#!/usr/bin/env python3
"""Run isolated Latch CLI benchmark cases and independent acceptance checks."""

import argparse
import datetime as dt
import hashlib
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parent
CASES = ROOT / "cases"
TIERS = ("easy", "medium", "hard")
PROVIDER = "opencode-go"
MODEL = "deepseek-v4.1-flash"


def discover():
    cases = {}
    for tier in TIERS:
        for manifest in sorted((CASES / tier).glob("*/case.json")):
            data = json.loads(manifest.read_text(encoding="utf-8"))
            if data["id"] in cases or data["tier"] != tier:
                raise ValueError(f"invalid or duplicate case: {manifest}")
            case_dir = manifest.parent
            if not (case_dir / "workspace").is_dir() or not any(
                (case_dir / name).is_file() for name in ("check.py", "acceptance.json")
            ):
                raise ValueError(f"case is incomplete: {case_dir}")
            cases[data["id"]] = (data, case_dir)
    return cases


def save_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def git_revision(directory):
    result = subprocess.run(["git", "rev-parse", "HEAD"], cwd=directory,
                            text=True, capture_output=True, check=True)
    return result.stdout.strip()


def case_digest(case_dir):
    digest = hashlib.sha256()
    for path in sorted(case_dir.rglob("*")):
        if path.is_file() and "__pycache__" not in path.parts and path.suffix != ".pyc":
            digest.update(path.relative_to(case_dir).as_posix().encode("utf-8"))
            digest.update(b"\0")
            digest.update(path.read_bytes())
            digest.update(b"\0")
    return digest.hexdigest()


def run_process(command, *, cwd, timeout, env=None):
    start = time.monotonic()
    process = subprocess.Popen(
        command, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, start_new_session=True,
    )
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(process.pid, signal.SIGTERM)
        try:
            stdout, stderr = process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate()
    return {
        "exit_code": process.returncode,
        "timed_out": timed_out,
        "wall_seconds": round(time.monotonic() - start, 3),
        "stdout": stdout,
        "stderr": stderr,
    }


def check(case_dir, workspace):
    bwrap = shutil.which("bwrap")
    if bwrap is None:
        raise RuntimeError("bwrap is required to isolate acceptance checks")
    data_driven = (case_dir / "acceptance.json").is_file()
    checker = "/tmp/check_case.py" if data_driven else "/tmp/case/check.py"
    checker_args = ["/tmp/case", "/tmp/workspace"] if data_driven else ["/tmp/workspace"]
    command = [
        bwrap, "--die-with-parent", "--new-session", "--unshare-user",
        "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--unshare-cgroup-try",
        "--unshare-net", "--ro-bind", "/", "/", "--dev", "/dev",
        "--proc", "/proc", "--tmpfs", "/tmp", "--tmpfs", "/run",
        "--tmpfs", "/home", "--ro-bind", str(case_dir), "/tmp/case",
        "--ro-bind", str(ROOT / "check_case.py"), "/tmp/check_case.py",
        "--ro-bind", str(workspace), "/tmp/workspace",
        "--clearenv", "--setenv", "HOME", "/tmp",
        "--setenv", "PATH", "/usr/bin:/bin",
        "--setenv", "PYTHONDONTWRITEBYTECODE", "1",
        "--chdir", "/tmp/workspace", "--", "/usr/bin/python3",
        checker, *checker_args,
    ]
    outcome = run_process(command, cwd=ROOT, timeout=30)
    if outcome["exit_code"] != 0 or outcome["timed_out"]:
        raise RuntimeError(f"evaluator failed: {outcome['stderr'][-1000:]}")
    data = json.loads(outcome["stdout"])
    checks = data["checks"]
    if not checks or any(not isinstance(item.get("passed"), bool) for item in checks):
        raise ValueError("evaluator returned invalid checks")
    return checks


def init_workspace(workspace):
    for args in (
        ["init", "-q"],
        ["add", "-A"],
        ["-c", "user.name=Latch Benchmark", "-c", "user.email=benchmark@localhost",
         "commit", "-qm", "baseline"],
    ):
        result = subprocess.run(["git", *args], cwd=workspace, text=True,
                                capture_output=True, check=False)
        if result.returncode:
            raise RuntimeError(f"git {' '.join(args)}: {result.stderr}")
    return git_revision(workspace)


def isolated_config(source, destination, state_dir):
    raw = source.read_text(encoding="utf-8")
    parsed = tomllib.loads(raw)
    if "state_dir" in parsed:
        raise ValueError("source config has a top-level state_dir; use a copy without it")
    destination.write_text(f"state_dir = {json.dumps(str(state_dir))}\n" + raw,
                           encoding="utf-8")
    destination.chmod(0o600)


def metrics(cli_result, pricing):
    if not cli_result:
        return None
    usage = cli_result.get("usage", {}).get("graph", {})
    input_tokens = usage.get("input_tokens")
    cache_read = usage.get("cache_read_tokens")
    output_tokens = usage.get("output_tokens")
    estimated_cost = None
    if (pricing and isinstance(input_tokens, int) and isinstance(output_tokens, int)
            and isinstance(cache_read, int) and 0 <= cache_read <= input_tokens):
        estimated_cost = round((
            (input_tokens - cache_read) * pricing["input_per_million"]
            + cache_read * pricing["cached_input_per_million"]
            + output_tokens * pricing["output_per_million"]
        ) / 1_000_000, 8)
    return {
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cache_read_tokens": cache_read,
        "cache_write_tokens": usage.get("cache_write_tokens"),
        "cache_read_fraction": round(cache_read / input_tokens, 4)
        if isinstance(cache_read, int) and isinstance(input_tokens, int) and input_tokens else None,
        "estimated_cost_usd": estimated_cost,
    }


def event_counts(state_dir, session_id):
    database = state_dir / "latch.sqlite3"
    if not session_id or not database.is_file():
        return None
    counts = {"model_turns": 0, "tool_calls": 0, "validation_passes": 0,
              "validation_failures": 0}
    with sqlite3.connect(database) as connection:
        for kind, payload in connection.execute(
            "SELECT kind, payload FROM events WHERE session_id=?", (session_id,)
        ):
            if kind == "model_request_started":
                counts["model_turns"] += 1
            elif kind == "tool_requested":
                counts["tool_calls"] += 1
            elif kind == "validation_result":
                passed = json.loads(payload)["data"]["passed"]
                counts["validation_passes" if passed else "validation_failures"] += 1
    return counts


def workspace_patch(workspace, baseline_commit):
    tracked = subprocess.run(
        ["git", "diff", "--binary", baseline_commit], cwd=workspace,
        text=True, capture_output=True, check=True,
    ).stdout
    untracked = subprocess.run(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"],
        cwd=workspace, capture_output=True, check=True,
    ).stdout
    parts = [tracked]
    for raw_path in filter(None, untracked.split(b"\0")):
        path = raw_path.decode("utf-8", errors="surrogateescape")
        diff = subprocess.run(
            ["git", "diff", "--no-index", "--binary", "--", "/dev/null", path],
            cwd=workspace, text=True, capture_output=True, check=False,
        )
        if diff.returncode != 1:
            raise RuntimeError(f"cannot capture untracked file {path}: {diff.stderr}")
        parts.append(diff.stdout)
    return "".join(parts)


def execute_case(data, case_dir, run_root, latch, source_config, pricing):
    case_root = run_root / data["id"]
    case_root.mkdir(mode=0o700)
    workspace = case_root / "workspace"
    shutil.copytree(case_dir / "workspace", workspace,
                    ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
    (workspace / ".gitignore").write_text("__pycache__/\n*.pyc\n", encoding="utf-8")
    baseline = check(case_dir, workspace)
    if all(item["passed"] for item in baseline):
        raise ValueError(f"{data['id']} has no failing baseline check")
    baseline_commit = init_workspace(workspace)
    isolated_config(source_config, case_root / "config.toml", case_root / "state")
    command = [
        str(latch), "run", "--config", str(case_root / "config.toml"),
        "--workspace", str(workspace), "--provider", PROVIDER, "--model", MODEL,
        "--mode", "WORK", "--prompt", data["prompt"], "--output", "json",
    ]
    outcome = run_process(command, cwd=workspace, timeout=data["timeout_seconds"])
    (case_root / "latch.stdout.json").write_text(outcome["stdout"], encoding="utf-8")
    (case_root / "latch.stderr.log").write_text(outcome["stderr"], encoding="utf-8")
    try:
        cli_result = json.loads(outcome["stdout"])
    except json.JSONDecodeError:
        cli_result = None
    after = check(case_dir, workspace)
    git_status = subprocess.run(["git", "status", "--short"], cwd=workspace,
                                text=True, capture_output=True, check=True).stdout.splitlines()
    (case_root / "change.patch").write_text(
        workspace_patch(workspace, baseline_commit), encoding="utf-8"
    )
    profile = (cli_result or {}).get("profile", {})
    passed = (
        outcome["exit_code"] == 0 and not outcome["timed_out"]
        and all(item["passed"] for item in after)
        and profile.get("provider") == PROVIDER and profile.get("model") == MODEL
    )
    report = {
        "case_id": data["id"], "tier": data["tier"], "title": data["title"],
        "case_sha256": case_digest(case_dir),
        "baseline_commit": baseline_commit,
        "passed": passed,
        "baseline_checks": baseline, "final_checks": after,
        "checks_passed": sum(item["passed"] for item in after),
        "checks_total": len(after),
        "cli_exit_code": outcome["exit_code"], "timed_out": outcome["timed_out"],
        "wall_seconds": outcome["wall_seconds"],
        "status": (cli_result or {}).get("status"),
        "completion": (cli_result or {}).get("task", {}).get("completion"),
        "session_id": (cli_result or {}).get("session_id"),
        "profile": profile, "usage": metrics(cli_result, pricing),
        "event_counts": event_counts(case_root / "state", (cli_result or {}).get("session_id")),
        "git_status": git_status,
        "workspace": str(workspace),
    }
    save_json(case_root / "result.json", report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("list", help="list available cases")
    run = sub.add_parser("run", help="run one or more cases with the real Latch CLI")
    run.add_argument("--case", action="append", required=True,
                     help="case ID; repeat for several cases or use 'all'")
    run.add_argument("--latch", type=Path, default=ROOT.parent / "target/release/latch")
    run.add_argument("--config", type=Path, required=True,
                     help="source Latch config; credential should use an environment variable")
    run.add_argument("--output-dir", type=Path, default=ROOT / "runs")
    run.add_argument("--pricing", type=Path,
                     help="JSON price snapshot with USD per million input, cached input, and output tokens")
    args = parser.parse_args()
    cases = discover()
    if args.command == "list":
        for data, _ in cases.values():
            print(f"{data['tier']:6} {data['id']:18} {data['title']}")
        return 0
    selected = list(cases) if args.case == ["all"] else args.case
    if not selected or any(case not in cases for case in selected) or len(set(selected)) != len(selected):
        parser.error(f"unknown case; available: {', '.join(cases)}")
    latch = args.latch.resolve()
    config = args.config.resolve()
    if not latch.is_file() or not config.is_file():
        parser.error("--latch and --config must point to existing files")
    pricing = None
    if args.pricing:
        pricing = json.loads(args.pricing.read_text(encoding="utf-8"))
        required = ("input_per_million", "cached_input_per_million", "output_per_million")
        if any(not isinstance(pricing.get(key), (int, float)) or pricing[key] < 0
               for key in required):
            parser.error("--pricing needs nonnegative USD rates per million tokens")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_root = args.output_dir.resolve() / f"{stamp}-{os.getpid()}"
    run_root.mkdir(mode=0o700)
    reports = []
    for case_id in selected:
        data, case_dir = cases[case_id]
        print(f"running {case_id} ({data['tier']})...", flush=True)
        try:
            report = execute_case(data, case_dir, run_root, latch, config, pricing)
            reports.append(report)
            print(f"  {report['checks_passed']}/{report['checks_total']} checks; "
                  f"completion={report['completion']}; {report['wall_seconds']}s", flush=True)
        except Exception as error:
            reports.append({"case_id": case_id, "tier": data["tier"],
                            "passed": False, "harness_error": str(error)})
            print(f"  harness error: {error}", file=sys.stderr, flush=True)
    summary = {
        "schema_version": 1, "provider": PROVIDER, "model": MODEL,
        "created_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "benchmark_commit": git_revision(ROOT),
        "latch": str(latch),
        "latch_sha256": hashlib.sha256(latch.read_bytes()).hexdigest(),
        "config_sha256": hashlib.sha256(config.read_bytes()).hexdigest(),
        "pricing": pricing, "cases": reports,
        "passed": sum(item["passed"] for item in reports),
        "total": len(reports),
        "checks_passed": sum(item.get("checks_passed", 0) for item in reports),
        "checks_total": sum(item.get("checks_total", 0) for item in reports),
        "total_wall_seconds": round(sum(item.get("wall_seconds", 0) for item in reports), 3),
    }
    save_json(run_root / "summary.json", summary)
    print(f"reports: {run_root}")
    return 0 if summary["passed"] == summary["total"] else 1


if __name__ == "__main__":
    sys.exit(main())
