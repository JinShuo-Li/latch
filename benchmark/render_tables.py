#!/usr/bin/env python3
"""Render the recorded comparison as publication-style Times New Roman tables."""

import argparse
import json
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.font_manager import FontProperties


ROOT = Path(__file__).resolve().parent


def draw_table(output, title, subtitle, rows, notes, regular, bold, breaks=()):
    height = 1.55 + len(rows) * .34 + len(notes) * .22
    figure, axes = plt.subplots(figsize=(10.8, height))
    figure.patch.set_facecolor("white")
    axes.set_axis_off()
    axes.set_position([0, 0, 1, 1])
    axes.set_xlim(0, 1)
    axes.set_ylim(0, height)
    left, right = .04, .96
    text = lambda x, y, value, **kw: axes.text(
        x, y, value, fontsize=12.2, fontproperties=regular,
        color="black", va="center", **kw)
    axes.text(left, height - .24, title, fontproperties=bold, fontsize=15.5, va="center")
    axes.text(left, height - .56, subtitle, fontproperties=regular, fontsize=11.4, va="center")
    top = height - .84
    axes.hlines(top, left, right, color="black", linewidth=1.25)
    for x, label, align in [(.05, "Metric", "left"), (.55, "Unit", "center"),
                            (.745, "Latch", "right"), (.95, "OpenCode", "right")]:
        axes.text(x, top - .22, label, fontsize=12.5, fontproperties=bold,
                  va="center", ha=align)
    axes.hlines(top - .44, left, right, color="black", linewidth=.75)
    for index, (metric, unit, latch, opencode, best) in enumerate(rows):
        y = top - .64 - index * .34
        text(.05, y, metric, ha="left")
        text(.55, y, unit, ha="center")
        for x, value, agent in [(.745, latch, "latch"), (.95, opencode, "opencode")]:
            axes.text(x, y, value, fontsize=12.2, va="center", ha="right",
                      fontproperties=bold if best == agent else regular)
        if index in breaks:
            axes.hlines(y - .17, left, right, color="#888888", linewidth=.4)
    bottom = top - .64 - (len(rows) - 1) * .34 - .22
    axes.hlines(bottom, left, right, color="black", linewidth=1.25)
    for index, note in enumerate(notes):
        axes.text(left, bottom - .25 - index * .22, note,
                  fontproperties=regular, fontsize=10.4, va="center")
    for suffix in ["svg", "pdf", "png"]:
        figure.savefig(output.with_suffix("." + suffix), dpi=240, facecolor="white",
                       metadata={"Creator": "Latch benchmark/render_tables.py"}
                       if suffix == "pdf" else None)
    plt.close(figure)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--font-dir", type=Path, default=Path("/mnt/c/Windows/Fonts"))
    parser.add_argument("--output-dir", type=Path, default=ROOT / "figures/2026-10-06")
    args = parser.parse_args()
    regular = FontProperties(fname=str(args.font_dir / "times.ttf"))
    bold = FontProperties(fname=str(args.font_dir / "timesbd.ttf"))
    if regular.get_name() != "Times New Roman" or bold.get_name() != "Times New Roman":
        raise ValueError("actual Times New Roman regular and bold fonts are required")
    plt.rcParams.update({"svg.fonttype": "path", "pdf.fonttype": 42,
                         "svg.hashsalt": "latch-benchmark-tables-2026-10-06"})
    metrics = json.loads((ROOT / "reports/2026-10-06-full-comparison/metrics.json").read_text())
    a, b = metrics["overall"]["latch"], metrics["overall"]["opencode"]
    paired = metrics["paired_both_passed"]
    rows = [
        ("Tasks passed", "n / N", f"{a['passed']}/25 (92%)", f"{b['passed']}/25 (88%)", "latch"),
        ("Independent checks passed", "n / N", f"{a['checks_passed']}/83", f"{b['checks_passed']}/83", "latch"),
        ("Timeouts", "count", str(a["timeouts"]), str(b["timeouts"]), None),
    ]
    for label, key in [("Mean wall time", "mean"), ("Median wall time", "median"),
                       ("90th-percentile wall time", "p90_nearest_rank")]:
        rows.append((label, "s", f"{a['wall_seconds_all'][key]:.2f}",
                     f"{b['wall_seconds_all'][key]:.2f}", "latch"))
    rows.append(("Mean wall time: paired passes (n = 22)", "s",
                 f"{paired['latch']['wall_seconds_all']['mean']:.2f}",
                 f"{paired['opencode']['wall_seconds_all']['mean']:.2f}", "latch"))
    for label, key in [("Total input (including cache)", "input_tokens"),
                       ("Total output (including reasoning)", "output_tokens"),
                       ("Total tokens", "total_tokens")]:
        rows.append((label, "tokens", f"{a['tokens'][key]:,}", f"{b['tokens'][key]:,}", "latch"))
    rows.append(("Mean tokens per task", "tokens", f"{a['tokens_per_attempt']['mean']:,.0f}",
                 f"{b['tokens_per_attempt']['mean']:,.0f}", "latch"))
    rows.append(("Input cache-hit fraction", "%", f"{a['tokens']['cache_hit_fraction'] * 100:.1f}",
                 f"{b['tokens']['cache_hit_fraction'] * 100:.1f}", "opencode"))
    for label, key in [("Model turns", "model_turns"), ("Tool calls", "tool_calls")]:
        rows.append((label, "count", f"{a[key]:,}", f"{b[key]:,}", "latch"))
    args.output_dir.mkdir(parents=True, exist_ok=True)
    draw_table(args.output_dir / "performance-table",
               "Table 1. Task quality and execution efficiency",
               "25 candidate tasks per agent | DeepSeek V4.1 Flash | Linux | 6 October 2026",
               rows, [
                   "One attempt per task; at most three attempts running globally. All-attempt means include failures.",
                   "P90 uses nearest rank. Paired passes contain the same 22 tasks. Usage totals include child sessions.",
                   "Bold marks favorable point estimates, not statistical significance. CPU/RSS are not comparable here.",
               ], regular, bold, breaks=(2, 6))
    fees = []
    for metric, period, scope, key in [
        ("Mean uncached-input cost (off-peak)", "off_peak", "mean_per_measured_attempt", "input_miss"),
        ("Mean cache-hit cost (off-peak)", "off_peak", "mean_per_measured_attempt", "input_hit"),
        ("Mean output cost (off-peak)", "off_peak", "mean_per_measured_attempt", "output"),
        ("Mean total cost per task (off-peak)", "off_peak", "mean_per_measured_attempt", "total"),
        ("Mean total cost per task (peak)", "peak", "mean_per_measured_attempt", "total"),
        ("Total cost: 25 attempts (off-peak)", "off_peak", "total", "total"),
        ("Total cost: 25 attempts (peak)", "peak", "total", "total"),
    ]:
        values = [stat["cost_scenarios"]["CNY"][period][scope][key] for stat in [a, b]]
        fees.append((metric, "CNY", f"{values[0]:.6f}", f"{values[1]:.6f}",
                     "latch" if values[0] < values[1] else "opencode"))
    values = [stat["cost_scenarios"]["CNY"]["off_peak"]["all_attempt_cost_per_success"] for stat in [a, b]]
    fees.append(("Cost per passed task (off-peak)", "CNY", f"{values[0]:.6f}", f"{values[1]:.6f}", "latch"))
    draw_table(args.output_dir / "cost-table", "Table 2. Estimated direct-API expenditure",
               "DeepSeek V4.1 Flash | Official CNY price snapshot: 6 October 2026",
               fees, [
                   "Off-peak rates per million tokens: uncached input CNY 1; cache-hit input CNY 0.02; output CNY 4.",
                   "Peak rates are twice off-peak rates. Means include all 25 attempts, including failed tasks.",
                   "Cost per passed task divides all-attempt expenditure by passes (Latch: 23; OpenCode: 22).",
                   "Estimates preserve observed cache hits. These are hypothetical DeepSeek API costs, not Go invoices.",
               ], regular, bold, breaks=(2, 4, 6))
    print("Rendered Times New Roman SVG, PDF and PNG tables:", args.output_dir)


if __name__ == "__main__":
    main()
