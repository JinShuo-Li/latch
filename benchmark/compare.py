#!/usr/bin/env python3
"""Run all candidate cases for Latch and OpenCode with one concurrency cap."""

import argparse
import concurrent.futures
import datetime as dt
import hashlib
import json
import os
import random
import shutil
import sqlite3
import threading
import time
import tomllib
from pathlib import Path

import run
import run_opencode


def resource_metrics(path):
    if not path.is_file():
        return None
    fields = {}
    for line in path.read_text().splitlines():
        key, separator, value = line.strip().rpartition(": ")
        if separator:
            fields[key] = value
    try:
        return {
            "cpu_user_seconds": float(fields["User time (seconds)"]),
            "cpu_system_seconds": float(fields["System time (seconds)"]),
            "max_rss_kib": int(fields["Maximum resident set size (kbytes)"]),
            "major_page_faults": int(fields["Major (requiring I/O) page faults"]),
            "measurement": "GNU time -v; maximum child RSS, not simultaneous tree RSS",
        }
    except (KeyError, ValueError):
        return None


def partial_latch_metrics(attempt):
    database = attempt / "state/latch.sqlite3"
    if not database.is_file():
        return None, None
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
        rows = connection.execute("SELECT kind,payload FROM events").fetchall()
    usages = [json.loads(payload)["data"]["usage"] for kind, payload in rows
              if kind == "model_usage"]
    counts = {"model_turns": sum(k == "model_request_started" for k, _ in rows),
              "tool_calls": sum(k == "tool_requested" for k, _ in rows),
              "validation_passes": 0, "validation_failures": 0}
    for kind, payload in rows:
        if kind == "validation_result":
            key = "validation_passes" if json.loads(payload)["data"]["passed"] else "validation_failures"
            counts[key] += 1
    if not usages:
        return None, counts
    usage = {key: sum(u.get(key) or 0 for u in usages)
             for key in ["input_tokens", "output_tokens", "cache_read_tokens",
                         "cache_write_tokens", "reasoning_tokens"]}
    usage["estimated_cost_usd"] = None
    usage["measurement"] = "partial durable usage; unfinished provider requests may be absent"
    return usage, counts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--latch-config", type=Path, required=True)
    parser.add_argument("--latch", type=Path, default=run.ROOT.parent / "target/release/latch")
    parser.add_argument("--opencode", type=Path, default=Path(shutil.which("opencode") or "opencode"))
    parser.add_argument("--jobs", choices=[1, 2, 3], type=int, default=3)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    credential = os.environ.get("OPENCODE_GO_API_KEY")
    if not credential or not shutil.which("bwrap") or not Path("/usr/bin/time").is_file():
        parser.error("OPENCODE_GO_API_KEY, Bubblewrap and GNU /usr/bin/time are required")
    config = args.latch_config.resolve()
    parsed = tomllib.loads(config.read_text())
    provider = parsed.get("provider", {})
    if provider.get("api_key_env") != "OPENCODE_GO_API_KEY":
        parser.error("comparison requires Latch's credential to use OPENCODE_GO_API_KEY")
    if provider.get("base_url", "").rstrip("/") != "https://opencode.ai/zen/go/v1":
        parser.error("comparison requires the same OpenCode Go endpoint")
    if "state_dir" in parsed:
        parser.error("source config must not set state_dir")
    latch, opencode = args.latch.resolve(), args.opencode.resolve()
    if not latch.is_file() or not opencode.is_file():
        parser.error("agent binaries must exist")
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    root = (args.output_dir or run.ROOT / "runs" / f"comparison-{stamp}-{os.getpid()}").resolve()
    root.mkdir(parents=True, mode=0o700)
    for name in ["latch", "opencode"]:
        (root / name).mkdir(mode=0o700)
    version = run.run_process([str(opencode), "--version"], cwd=root, timeout=10,
                             env={"PATH": "/usr/bin:/bin", "XDG_DATA_HOME": str(root / "version/data"),
                                  "XDG_STATE_HOME": str(root / "version/state"),
                                  "XDG_CACHE_HOME": str(root / "version/cache")})
    if version["exit_code"] != 0:
        raise RuntimeError("cannot identify OpenCode version")
    cases = run.discover()
    rng = random.Random(args.seed)
    order = list(cases)
    rng.shuffle(order)
    schedule = []
    for case_id in order:
        agents = ["latch", "opencode"]
        rng.shuffle(agents)
        schedule.extend((agent, case_id) for agent in agents)
    metadata = {
        "source_revision": run.git_revision(run.ROOT), "started_at_utc": stamp,
        "max_concurrency_across_both_agents": args.jobs, "schedule_seed": args.seed,
        "schedule": schedule, "provider": run.PROVIDER, "model": run.MODEL,
        "latch_sha256": hashlib.sha256(latch.read_bytes()).hexdigest(),
        "opencode_sha256": hashlib.sha256(opencode.read_bytes()).hexdigest(),
        "opencode_version": version["stdout"].strip(),
        "latch_config_sha256": hashlib.sha256(config.read_bytes()).hexdigest(),
        "runner_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "case_sha256": {cid: run.case_digest(directory) for cid, (_, directory) in cases.items()},
        "attempts_per_agent_per_case": 1, "pricing": None,
    }
    run.save_json(root / "supervision.json", metadata)
    original_run_process = run.run_process
    measurements = {}
    lock = threading.Lock()

    def measured_process(command, *, cwd, timeout, env=None):
        is_agent = "run" in command and (str(latch) in command or "/tmp/bin/opencode" in command)
        if not is_agent:
            return original_run_process(command, cwd=cwd, timeout=timeout, env=env)
        path = Path(cwd).parent / "resources.txt"
        outcome = original_run_process(["/usr/bin/time", "-v", "-o", str(path), *command],
                                       cwd=cwd, timeout=timeout, env=env)
        with lock:
            measurements[str(Path(cwd).parent)] = resource_metrics(path)
        return outcome

    run.run_process = measured_process

    def execute(agent, case_id):
        start = time.monotonic()
        print("START", agent, case_id, flush=True)
        attempt = root / agent / case_id
        try:
            if agent == "latch":
                data, directory = cases[case_id]
                result = run.execute_case(data, directory, root / agent, latch, config, None)
                if result.get("usage") is None:
                    usage, counts = partial_latch_metrics(attempt)
                    result.update(usage=usage, event_counts=counts)
            else:
                result = run_opencode.execute(case_id, cases[case_id], root / agent, opencode, credential)
        except Exception as error:
            result = {"case_id": case_id, "tier": cases[case_id][0]["tier"],
                      "passed": False, "harness_error": str(error)}
        result["agent"] = agent
        result["attempt_total_seconds"] = round(time.monotonic() - start, 3)
        result["resources"] = measurements.get(str(attempt))
        run.save_json(attempt / "result.json", result)
        print("DONE", agent, case_id, "PASS" if result["passed"] else "FAIL",
              f"{result.get('checks_passed', '?')}/{result.get('checks_total', '?')}",
              f"{result.get('wall_seconds', '?')}s", flush=True)
        return result

    print("REPORT_ROOT", root, flush=True)
    started = time.monotonic()
    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = [pool.submit(execute, agent, case_id) for agent, case_id in schedule]
        for future in concurrent.futures.as_completed(futures):
            results.append(future.result())
            run.save_json(root / "progress.json", {"completed": len(results), "expected": len(schedule),
                          "cases": results})
    summary = {**metadata, "finished_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
               "batch_wall_seconds": round(time.monotonic() - started, 3),
               "cases": sorted(results, key=lambda r: (r["agent"], r["case_id"]))}
    run.save_json(root / "summary.json", summary)
    print("FINISHED", len(results), "attempts", root / "summary.json", flush=True)
    return 0 if all(r["passed"] for r in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
