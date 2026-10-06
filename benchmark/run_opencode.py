#!/usr/bin/env python3
"""Compare OpenCode on unchanged candidate cases using the shared evaluator."""

import argparse
import concurrent.futures
import datetime as dt
import hashlib
import json
import os
import shutil
import sqlite3
from pathlib import Path

import run


MODEL = "opencode-go/deepseek-v4.1-flash"


def sandbox_command(binary, attempt, arguments):
    return [
        shutil.which("bwrap"), "--die-with-parent", "--new-session",
        "--unshare-user", "--unshare-pid", "--unshare-ipc", "--unshare-uts",
        "--unshare-cgroup-try", "--ro-bind", "/", "/", "--dev", "/dev",
        "--proc", "/proc", "--tmpfs", "/home", "--tmpfs", "/tmp",
        "--tmpfs", "/run", "--bind", str(attempt), "/run/attempt",
        "--dir", "/tmp/bin", "--ro-bind", str(binary), "/tmp/bin/opencode",
        "--setenv", "HOME", "/run/attempt/home",
        "--setenv", "PATH", "/usr/local/bin:/usr/bin:/bin",
        "--setenv", "XDG_DATA_HOME", "/run/attempt/data",
        "--setenv", "XDG_CONFIG_HOME", "/run/attempt/config",
        "--setenv", "XDG_STATE_HOME", "/run/attempt/state",
        "--setenv", "XDG_CACHE_HOME", "/run/attempt/cache",
        "--chdir", "/run/attempt/workspace", "--", "/tmp/bin/opencode",
        *arguments,
    ]


def database_metrics(attempt):
    database = attempt / "data/opencode/opencode.db"
    if not database.is_file():
        return None, [], None
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
        tables = {r[0] for r in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        version_two = "session_message" in tables
        if version_two:
            assistants = [json.loads(row[0]) for row in connection.execute(
                "SELECT data FROM session_message WHERE type='assistant'")]
            tools = sum(p.get("type") == "tool" for m in assistants
                        for p in m.get("content", []))
            models = sorted({f"{m.get('model', {}).get('providerID')}/{m.get('model', {}).get('id')}"
                             for m in assistants})
        elif "message" in tables:
            messages = [json.loads(row[0]) for row in connection.execute("SELECT data FROM message")]
            assistants = [m for m in messages if m.get("role") == "assistant"]
            tools = sum(json.loads(row[0]).get("type") == "tool"
                        for row in connection.execute("SELECT data FROM part"))
            models = sorted({f"{m.get('providerID')}/{m.get('modelID')}" for m in assistants})
        else:
            return None, [], None
    # OpenCode's input excludes cache reads/writes; Latch's input includes them.
    tokens = [m["tokens"] for m in assistants if isinstance(m.get("tokens"), dict)]
    uncached = sum(t.get("input", 0) for t in tokens)
    cache_read = sum(t.get("cache", {}).get("read", 0) for t in tokens)
    cache_write = sum(t.get("cache", {}).get("write", 0) for t in tokens)
    output = sum(t.get("output", 0) for t in tokens)
    reasoning = sum(t.get("reasoning", 0) for t in tokens)
    usage = {
        "input_tokens": uncached + cache_read + cache_write,
        "uncached_input_tokens": uncached,
        "output_tokens": output + reasoning if version_two else output,
        "reported_output_tokens": output,
        "reasoning_tokens": reasoning,
        "cache_read_tokens": cache_read,
        "cache_write_tokens": cache_write,
        "estimated_cost_usd": None,
    } if tokens else None
    return usage, models, {"model_turns": len(assistants), "tool_calls": tools}


def execute(case_id, case, root, binary, credential):
    data, case_dir = case
    attempt = root / case_id
    attempt.mkdir(mode=0o700)
    workspace = attempt / "workspace"
    shutil.copytree(case_dir / "workspace", workspace,
                    ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
    (workspace / ".gitignore").write_text("__pycache__/\n*.pyc\n")
    baseline_checks = run.check(case_dir, workspace)
    if all(c["passed"] for c in baseline_checks):
        raise ValueError(f"{case_id}: baseline must fail")
    baseline = run.init_workspace(workspace)
    for directory in ["home", "data/opencode", "config/opencode", "state", "cache"]:
        (attempt / directory).mkdir(parents=True, exist_ok=True)
    config = {"model": MODEL, "small_model": MODEL, "autoupdate": False,
              "share": "disabled", "permission": "allow",
              "provider": {"opencode-go": {
                  "npm": "@ai-sdk/openai-compatible",
                  "options": {"baseURL": "https://opencode.ai/zen/go/v1",
                              "apiKey": "{env:OPENCODE_GO_API_KEY}"},
                  "models": {"deepseek-v4.1-flash": {"name": "DeepSeek v4.1 Flash",
                              "reasoning": True,
                              "interleaved": {"field": "reasoning_content"},
                              "limit": {"context": 256000, "output": 32768}}}}}}
    run.save_json(attempt / "config/opencode/opencode.json", config)
    outcome = run.run_process(
        sandbox_command(binary, attempt, ["run", "--standalone", "--auto",
                        "--agent", "build", "--model", MODEL, "--format", "json",
                        "--title", f"Benchmark {case_id}", data["prompt"]]),
        cwd=workspace, timeout=data["timeout_seconds"],
        env={"PATH": "/usr/bin:/bin", "OPENCODE_GO_API_KEY": credential},
    )
    (attempt / "opencode.stdout.jsonl").write_text(outcome["stdout"])
    (attempt / "opencode.stderr.log").write_text(outcome["stderr"])
    after = run.check(case_dir, workspace)
    usage, models, events = database_metrics(attempt)
    result = {
        "case_id": case_id, "tier": data["tier"],
        "case_sha256": run.case_digest(case_dir), "baseline_commit": baseline,
        "passed": (outcome["exit_code"] == 0 and not outcome["timed_out"]
                   and all(c["passed"] for c in after) and models == [MODEL]),
        "baseline_checks": baseline_checks, "final_checks": after,
        "checks_passed": sum(c["passed"] for c in after), "checks_total": len(after),
        "cli_exit_code": outcome["exit_code"], "timed_out": outcome["timed_out"],
        "wall_seconds": outcome["wall_seconds"], "models": models,
        "usage": usage, "event_counts": events,
    }
    (attempt / "change.patch").write_text(run.workspace_patch(workspace, baseline))
    run.save_json(attempt / "result.json", result)
    print(case_id, "PASS" if result["passed"] else "FAIL",
          f"{result['checks_passed']}/{result['checks_total']}",
          f"{result['wall_seconds']}s", flush=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--case", action="append", required=True)
    parser.add_argument("--jobs", type=int, choices=[1, 2, 3], default=3)
    parser.add_argument("--opencode", default=shutil.which("opencode"))
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    if not args.opencode or not shutil.which("bwrap"):
        parser.error("opencode and bwrap are required")
    credential = os.environ.get("OPENCODE_GO_API_KEY")
    if not credential:
        parser.error("OPENCODE_GO_API_KEY is required (same credential as Latch)")
    cases = run.discover()
    selected = list(dict.fromkeys(args.case))
    if any(case_id not in cases for case_id in selected):
        parser.error("unknown case")
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    root = args.output_dir or run.ROOT / "runs" / f"opencode-{stamp}-{os.getpid()}"
    root = root.resolve()
    root.mkdir(parents=True, mode=0o700)
    binary = Path(args.opencode).resolve()
    version = run.run_process([str(binary), "--version"], cwd=root, timeout=10,
                             env={"PATH": "/usr/bin:/bin",
                                  "XDG_DATA_HOME": str(root / "version/data"),
                                  "XDG_STATE_HOME": str(root / "version/state"),
                                  "XDG_CACHE_HOME": str(root / "version/cache")})
    if version["exit_code"] != 0:
        raise RuntimeError("cannot identify installed OpenCode version")
    metadata = {"started_at_utc": stamp, "source_revision": run.git_revision(run.ROOT),
                "opencode_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "opencode_version": version["stdout"].strip(),
                "model": MODEL, "max_concurrency": args.jobs,
                "sandbox": "whole OpenCode process in Bubblewrap; host home hidden",
                "config": {"agent": "build", "permissions": "auto", "standalone": True}}
    run.save_json(root / "supervision.json", metadata)
    print("REPORT_ROOT", root, flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = [pool.submit(execute, case_id, cases[case_id], root, binary, credential)
                   for case_id in selected]
        results = [f.result() for f in futures]
    run.save_json(root / "summary.json", {**metadata, "cases": results,
                  "passed": sum(r["passed"] for r in results), "total": len(results),
                  "finished_at_utc": dt.datetime.now(dt.timezone.utc).isoformat()})
    return 0 if all(r["passed"] for r in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
