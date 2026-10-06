#!/usr/bin/env python3
"""Export full comparison evidence, performance metrics and priced scenarios."""

import argparse
import csv
import gzip
import hashlib
import io
import json
import math
import os
import sqlite3
import statistics
from collections import Counter
from pathlib import Path

import compare
import run


def distribution(values):
    values = sorted(v for v in values if isinstance(v, (float, int)))
    if not values:
        return {"samples": 0}
    return {"samples": len(values), "sum": sum(values), "mean": statistics.mean(values),
            "median": statistics.median(values), "p90_nearest_rank": values[math.ceil(len(values) * .9) - 1],
            "min": values[0], "max": values[-1]}


def costs(usage, rates):
    if not usage or any(not isinstance(usage.get(k), int)
                        for k in ["input_tokens", "output_tokens", "cache_read_tokens"]):
        return None
    hit = usage["cache_read_tokens"]
    miss = usage["input_tokens"] - hit
    if miss < 0:
        raise ValueError("cache exceeds total input")
    amounts = {"input_miss": miss * rates["input_miss"] / 1e6,
               "input_hit": hit * rates["input_hit"] / 1e6,
               "output": usage["output_tokens"] * rates["output"] / 1e6}
    return {**amounts, "total": sum(amounts.values())}


def aggregate(rows, prices):
    usage_rows = [r for r in rows if r.get("usage")]
    tokens = {k: sum(r["usage"].get(k) or 0 for r in usage_rows)
              for k in ["input_tokens", "output_tokens", "cache_read_tokens"]}
    tokens["total_tokens"] = tokens["input_tokens"] + tokens["output_tokens"]
    tokens["uncached_input_tokens"] = tokens["input_tokens"] - tokens["cache_read_tokens"]
    tokens["cache_hit_fraction"] = tokens["cache_read_tokens"] / tokens["input_tokens"] if tokens["input_tokens"] else None
    passed = sum(r["passed"] for r in rows)
    result = {
        "attempts": len(rows), "passed": passed, "failed": len(rows) - passed,
        "checks_passed": sum(r.get("checks_passed", 0) for r in rows),
        "checks_total": sum(r.get("checks_total", 0) for r in rows),
        "timeouts": sum(bool(r.get("timed_out")) for r in rows),
        "harness_errors": sum("harness_error" in r for r in rows),
        "completion_states": dict(Counter(r.get("completion", "not_applicable") for r in rows)),
        "usage_missing_attempts": len(rows) - len(usage_rows), "tokens": tokens,
        "wall_seconds_all": distribution([r.get("wall_seconds") for r in rows]),
        "wall_seconds_passed": distribution([r.get("wall_seconds") for r in rows if r["passed"]]),
        "tokens_per_attempt": distribution([r["usage"]["input_tokens"] + r["usage"]["output_tokens"] for r in usage_rows]),
        "model_turns": sum((r.get("event_counts") or {}).get("model_turns", 0) for r in rows),
        "tool_calls": sum((r.get("event_counts") or {}).get("tool_calls", 0) for r in rows),
        "resources": {key: distribution([(r.get("resources") or {}).get(key) for r in rows])
                      for key in ["cpu_user_seconds", "cpu_system_seconds", "max_rss_kib"]},
        "cost_scenarios": {},
    }
    for currency, periods in prices["rates_per_million"].items():
        result["cost_scenarios"][currency] = {}
        for period, rates in periods.items():
            measured = [costs(r.get("usage"), rates) for r in rows]
            measured = [c for c in measured if c is not None]
            totals = {k: sum(c[k] for c in measured) for k in ["input_miss", "input_hit", "output", "total"]}
            result["cost_scenarios"][currency][period] = {
                "measured_attempts": len(measured), "total": totals,
                "mean_per_measured_attempt": {k: v / len(measured) for k, v in totals.items()} if measured else None,
                "all_attempt_cost_per_success": totals["total"] / passed if passed else None,
            }
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_root", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    source, output = args.run_root.resolve(), args.output.resolve()
    summary = json.loads((source / "summary.json").read_text())
    prices = json.loads((source / "deepseek-pricing.json").read_text())
    rows = summary["cases"]
    assert len(rows) == 50 and len({(r["agent"], r["case_id"]) for r in rows}) == 50
    assert Counter(r["agent"] for r in rows) == {"latch": 25, "opencode": 25}
    secrets = [v.encode() for k, v in os.environ.items()
               if any(s in k for s in ["API_KEY", "TOKEN", "SECRET"]) and len(v) >= 16]

    def write(path, data):
        if any(secret in data for secret in secrets):
            raise RuntimeError("credential material in export")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def save(path, data):
        write(path, (json.dumps(data, indent=2) + "\n").encode())

    for r in rows:
        attempt = source / r["agent"] / r["case_id"]
        if r["agent"] == "latch":
            usage, counts = compare.partial_latch_metrics(attempt)
            r["root_event_counts"] = r.get("event_counts")
            if counts:
                r["event_counts"] = counts
            if r.get("usage") is None:
                r["usage"] = usage
        other = next(x for x in rows if x["case_id"] == r["case_id"] and x["agent"] != r["agent"])
        if r.get("case_sha256") and other.get("case_sha256"):
            assert r["case_sha256"] == other["case_sha256"]
        r["cost_scenarios"] = {currency: {period: costs(r.get("usage"), rate) for period, rate in periods.items()}
                               for currency, periods in prices["rates_per_million"].items()}
        out = output / "cases" / r["agent"] / r["case_id"]
        save(out / "result.json", r)
        for name in ["change.patch", "latch.stdout.json", "latch.stderr.log", "opencode.stdout.jsonl",
                     "opencode.stderr.log", "resources.txt"]:
            if (attempt / name).exists():
                write(out / name, (attempt / name).read_bytes())
        db = attempt / ("state/latch.sqlite3" if r["agent"] == "latch" else "data/opencode/opencode.db")
        if db.exists():
            with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as connection:
                connection.row_factory = sqlite3.Row
                table = "events" if r["agent"] == "latch" else "session_message"
                records = [dict(record) for record in connection.execute(f"SELECT * FROM {table}")]
                for record in records:
                    field = "payload" if table == "events" else "data"
                    record[field] = json.loads(record[field])
            data = ("\n".join(json.dumps(record) for record in records) + "\n").encode()
            if any(secret in data for secret in secrets):
                raise RuntimeError("credential in durable records")
            write(out / "events.jsonl.gz", gzip.compress(data, mtime=0))
    stats = {agent: aggregate([r for r in rows if r["agent"] == agent], prices) for agent in ["latch", "opencode"]}
    tiers = {tier: {agent: aggregate([r for r in rows if r["agent"] == agent and r["tier"] == tier], prices)
                    for agent in stats} for tier in run.TIERS}
    save(output / "summary.json", summary)
    save(output / "metrics.json", {"overall": stats, "tiers": tiers})
    for name in ["deepseek-pricing.json", "host.json"]:
        write(output / name, (source / name).read_bytes())
    buffer = io.StringIO()
    csv_rows = []
    for r in rows:
        u = r.get("usage") or {}
        e = r.get("event_counts") or {}
        csv_rows.append({k: r.get(k) for k in ["agent", "case_id", "tier", "passed", "checks_passed", "checks_total", "wall_seconds", "timed_out", "completion"]}
                        | {k: u.get(k) for k in ["input_tokens", "output_tokens", "cache_read_tokens"]}
                        | {k: e.get(k) for k in ["model_turns", "tool_calls"]}
                        | (r.get("resources") or {})
                        | {"estimated_cny_off_peak": (r["cost_scenarios"]["CNY"]["off_peak"] or {}).get("total"),
                           "estimated_cny_peak": (r["cost_scenarios"]["CNY"]["peak"] or {}).get("total")})
    fields = list(dict.fromkeys(k for r in csv_rows for k in r))
    writer = csv.DictWriter(buffer, fieldnames=fields)
    writer.writeheader()
    writer.writerows(csv_rows)
    write(output / "per-attempt.csv", buffer.getvalue().encode())
    lines = ["# Full 25-case Latch / OpenCode comparison", "",
             f"Source `{summary['source_revision']}`; OpenCode `{summary['opencode_version']}`. Same Linux host, model `{summary['model']}`, credential, prompts, fixtures, timeouts and external checks. One attempt per agent per case; at most three attempts globally. Randomized paired order, seed 42; exact schedule and hashes in summary.json.", "",
             "## Overall performance", "", "| Metric | Latch | OpenCode |", "| --- | ---: | ---: |"]
    a, b = stats["latch"], stats["opencode"]
    metrics = [("Passed cases", f"{a['passed']}/25", f"{b['passed']}/25"),
               ("External checks", f"{a['checks_passed']}/{a['checks_total']}", f"{b['checks_passed']}/{b['checks_total']}"),
               ("Timeouts", a["timeouts"], b["timeouts"]),
               ("Total input tokens (including cache)", a["tokens"]["input_tokens"], b["tokens"]["input_tokens"]),
               ("Total output tokens (including reasoning)", a["tokens"]["output_tokens"], b["tokens"]["output_tokens"]),
               ("Total tokens", a["tokens"]["total_tokens"], b["tokens"]["total_tokens"]),
               ("Cache-hit fraction", f"{a['tokens']['cache_hit_fraction']:.1%}", f"{b['tokens']['cache_hit_fraction']:.1%}"),
               ("Mean seconds / attempt", f"{a['wall_seconds_all']['mean']:.3f}", f"{b['wall_seconds_all']['mean']:.3f}"),
               ("Median seconds / attempt", f"{a['wall_seconds_all']['median']:.3f}", f"{b['wall_seconds_all']['median']:.3f}"),
               ("P90 seconds / attempt (nearest rank)", f"{a['wall_seconds_all']['p90_nearest_rank']:.3f}", f"{b['wall_seconds_all']['p90_nearest_rank']:.3f}"),
               ("Sum of task wall seconds", f"{a['wall_seconds_all']['sum']:.3f}", f"{b['wall_seconds_all']['sum']:.3f}"),
               ("Mean maximum-child RSS, MiB", f"{a['resources']['max_rss_kib'].get('mean', 0)/1024:.2f}", f"{b['resources']['max_rss_kib'].get('mean', 0)/1024:.2f}"),
               ("Model turns (graph)", a["model_turns"], b["model_turns"]),
               ("Tool calls (graph)", a["tool_calls"], b["tool_calls"])]
    lines.extend(f"| {label} | {av} | {bv} |" for label, av, bv in metrics)
    lines += ["", "## Cost estimate", "",
              "Prices retrieved 2026-10-06 from [DeepSeek CNY pricing](https://api-docs.deepseek.com/zh-cn/quick_start/pricing/) and [USD pricing](https://api-docs.deepseek.com/quick_start/pricing/). Per million tokens: off-peak CNY miss 1 / hit 0.02 / output 4; peak 2 / 0.04 / 8. USD off-peak 0.15 / 0.003 / 0.60; peak 0.30 / 0.006 / 1.20. These are separate official currency price lists, not FX conversions.", "",
              "Estimated cost = uncached input × miss rate + cached input × hit rate + total output × output rate, divided by 1,000,000. Cached tokens are already included in input; reasoning is already included in normalized output. These are hypothetical direct DeepSeek API prices applied to observed OpenCode Go usage, not actual Go invoices. Both time-of-day scenarios are shown; no billing period is assigned to Go calls.", "",
              "| Agent / rate | Mean uncached input cost | Mean cache-hit cost | Mean output cost | Mean total / attempt | Total / 25 attempts | All-attempt cost / passed case |",
              "| --- | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for agent, stat in stats.items():
        for period in ["off_peak", "peak"]:
            fee = stat["cost_scenarios"]["CNY"][period]
            mean = fee["mean_per_measured_attempt"]
            lines.append(f"| {agent} / {period} CNY | {mean['input_miss']:.6f} | {mean['input_hit']:.6f} | {mean['output']:.6f} | {mean['total']:.6f} | {fee['total']['total']:.6f} | {fee['all_attempt_cost_per_success']:.6f} |")
    lines += ["", "## Per-case results", "", "| Case | Latch checks | OpenCode checks | Latch passed | OpenCode passed | Latch seconds | OpenCode seconds | Latch tokens | OpenCode tokens |", "| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: |"]
    for cid in sorted({r["case_id"] for r in rows}):
        pair = {r["agent"]: r for r in rows if r["case_id"] == cid}
        x, y = pair["latch"], pair["opencode"]
        total = lambda r: (r.get("usage") or {}).get("input_tokens", 0) + (r.get("usage") or {}).get("output_tokens", 0)
        lines.append(f"| {cid} | {x.get('checks_passed')}/{x.get('checks_total')} | {y.get('checks_passed')}/{y.get('checks_total')} | {x['passed']} | {y['passed']} | {x.get('wall_seconds')} | {y.get('wall_seconds')} | {total(x)} | {total(y)} |")
    lines += ["", "## Interpretation and evidence", "",
              "One run per case is exploratory; random sampling, provider load and cache state can change results. Correctness uses independent acceptance plus successful CLI exit and exact model, not a comparison of Latch Verified with OpenCode termination. Failed cases remain included in overall averages; successful-only distributions and tier metrics are in metrics.json. All-attempt cost per pass includes spending on failed attempts, not the mean cost of passing attempts alone.", "",
              "Wall time is monotonic CLI elapsed time including cold startup, provider and tool waits; task sums are not parallel batch wall time. GNU time CPU and RSS include waited-for descendants; RSS is the largest single-process high-water mark, not simultaneous total process-tree memory. Missing resource measurements are excluded, never imputed as zero. No OS cache flush or provider phase attribution was performed.", "",
              "Token usage is provider-reported durable session activity. OpenCode v2 cache reads/writes are added to input and reasoning is added to output to match Latch's totals. If a timeout leaves a provider request unfinished, durable usage is a lower bound and the invoice may be higher. Pricing estimates hold observed cache hits constant; direct DeepSeek API could have different cache behavior. No claims of statistical significance or third-party benchmark validity are made.", "",
              f"Original isolated workspaces and databases remain in ignored `{source.relative_to(run.ROOT.parent)}`. Exported evidence excludes configs, credentials and databases. Per-attempt patches, stdout/stderr, resource logs and compressed durable records are under cases/. CSV includes input/output/cache usage and per-attempt estimated CNY cost; metrics.json includes USD/CNY scenarios and tier distributions.", ""]
    write(output / "README.md", "\n".join(lines).encode())
    files = sorted(p for p in output.rglob("*") if p.is_file() and p.name != "SHA256SUMS")
    write(output / "SHA256SUMS", "".join(hashlib.sha256(p.read_bytes()).hexdigest() + "  " + str(p.relative_to(output)) + "\n" for p in files).encode())
    print("Exported", len(files), "files", output)


if __name__ == "__main__":
    main()
