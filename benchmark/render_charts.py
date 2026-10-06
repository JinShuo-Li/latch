#!/usr/bin/env python3
"""Render transparent per-task benchmark charts from the recorded CSV."""

import argparse
import csv
import json
from pathlib import Path

import matplotlib

matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib.font_manager import FontProperties
from matplotlib.patches import Patch

ROOT = Path(__file__).resolve().parent
REPORT = ROOT / 'reports/2026-10-06-full-comparison'
COLORS = {'latch': '#56B4E9', 'opencode': '#E69F00'}


def load_records():
    with (REPORT / 'per-attempt.csv').open(newline='') as source:
        rows = list(csv.DictReader(source))
    records = {(row['agent'], row['case_id']): row for row in rows}
    cases = sorted({row['case_id'] for row in rows},
                   key=lambda case: ({'easy': 0, 'medium': 1, 'hard': 2}[
                       records['latch', case]['tier']], case))
    if len(rows) != 50 or len(records) != 50 or len(cases) != 25:
        raise ValueError('expected 25 unique paired tasks / 50 attempts')
    metrics = json.loads((REPORT / 'metrics.json').read_text())
    for agent in COLORS:
        paired = [records[agent, case] for case in cases]
        assert sum(int(row['input_tokens']) + int(row['output_tokens'])
                   for row in paired) == metrics['overall'][agent]['tokens']['total_tokens']
        assert abs(sum(float(row['wall_seconds']) for row in paired)
                   - metrics['overall'][agent]['wall_seconds_all']['sum']) < 1e-6
        assert sum(row['passed'] == 'True' for row in paired) == metrics['overall'][agent]['passed']
        assert abs(sum(float(row['estimated_cny_off_peak']) for row in paired)
                   - metrics['overall'][agent]['cost_scenarios']['CNY']['off_peak']['total']['total']) < 1e-9
    return records, cases


def draw_chart(output, title, unit, value, formatter, records, cases, regular, bold, theme):
    foreground = '#e6edf3' if theme == 'dark' else '#24292f'
    grid = '#8b949e' if theme == 'dark' else '#6e7781'
    figure, axes = plt.subplots(figsize=(11, 10.8))
    figure.subplots_adjust(left=.25, right=.95, top=.87, bottom=.10)
    figure.patch.set_alpha(0)
    axes.patch.set_alpha(0)
    maximum = max(value(records[agent, case]) for agent in COLORS for case in cases)
    for agent, offset in [('latch', -.19), ('opencode', .19)]:
        values = [value(records[agent, case]) for case in cases]
        bars = axes.barh([index + offset for index in range(len(cases))], values,
                         height=.34, color=COLORS[agent], zorder=3)
        for bar, case, number in zip(bars, cases, values):
            if records[agent, case]['passed'] != 'True':
                bar.set_hatch('///')
                bar.set_edgecolor(foreground)
                bar.set_linewidth(.6)
            axes.text(number + maximum * .009, bar.get_y() + bar.get_height() / 2,
                      formatter(number), va='center', color=foreground,
                      fontproperties=regular, fontsize=8.5)
    axes.set_yticks(range(len(cases)), cases)
    axes.set_ylim(len(cases) - .5, -.7)
    axes.set_xlim(0, maximum * 1.16)
    axes.set_xlabel(unit, fontproperties=regular, fontsize=12, color=foreground)
    axes.tick_params(axis='both', colors=foreground, labelsize=10, length=0, pad=6)
    for label in axes.get_xticklabels() + axes.get_yticklabels():
        label.set_fontproperties(regular)
    axes.set_axisbelow(True)
    axes.xaxis.grid(True, color=grid, alpha=.25, linewidth=.6)
    for spine in axes.spines.values():
        spine.set_visible(False)
    for index in range(1, len(cases)):
        if records['latch', cases[index]]['tier'] != records['latch', cases[index - 1]]['tier']:
            axes.axhline(index - .5, color=grid, alpha=.4, linewidth=.7)
    means = {agent: sum(value(records[agent, case]) for case in cases) / len(cases)
             for agent in COLORS}
    figure.text(.04, .97, title, fontproperties=bold, fontsize=17, color=foreground, va='top')
    figure.text(.04, .935,
                f"25 paired tasks | Mean: Latch {formatter(means['latch'])}; OpenCode {formatter(means['opencode'])} {unit}",
                fontproperties=regular, fontsize=11.5, color=foreground, va='top')
    handles = [Patch(facecolor=COLORS[agent], label=label) for agent, label in
               [('latch', 'Latch'), ('opencode', 'OpenCode')]]
    handles.append(Patch(facecolor='none', edgecolor=foreground, hatch='///', label='Failed task'))
    legend = figure.legend(handles=handles, loc='upper left', bbox_to_anchor=(.04, .915),
                           frameon=False, ncol=3, prop=regular, labelcolor=foreground)
    for label in legend.get_texts():
        label.set_fontsize(11)
    figure.text(.04, .04, 'One attempt per agent per task; at most three attempts globally. Failed attempts are included.',
                fontproperties=regular, fontsize=10, color=foreground)
    note = ('Input includes cache hits; output includes reasoning. Cost uses off-peak direct DeepSeek API rates.'
            if output.name.startswith('task-cost') else
            'Tasks ordered by difficulty (easy / medium / hard), then name. Hatching marks external-check failures.')
    figure.text(.04, .02, note, fontproperties=regular, fontsize=10, color=foreground)
    for suffix in ['svg', 'pdf', 'png']:
        figure.savefig(output.with_suffix('.' + suffix), transparent=True, dpi=200,
                       metadata={'Creator': 'Latch benchmark/render_charts.py'} if suffix == 'pdf' else None)
    plt.close(figure)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--font-dir', type=Path, default=Path('/mnt/c/Windows/Fonts'))
    parser.add_argument('--output-dir', type=Path, default=ROOT / 'figures/2026-10-06')
    args = parser.parse_args()
    regular = FontProperties(fname=str(args.font_dir / 'times.ttf'))
    bold = FontProperties(fname=str(args.font_dir / 'timesbd.ttf'))
    if any(font.get_name() != 'Times New Roman' for font in [regular, bold]):
        raise ValueError('actual Times New Roman regular and bold fonts are required')
    plt.rcParams.update({'svg.fonttype': 'path', 'pdf.fonttype': 42,
                         'svg.hashsalt': 'latch-benchmark-charts-2026-10-06'})
    records, cases = load_records()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    charts = [
        ('task-time', 'Per-task execution time', 'seconds', lambda row: float(row['wall_seconds']), lambda n: f'{n:.1f}'),
        ('task-tokens', 'Per-task total token usage', 'thousand tokens',
         lambda row: (int(row['input_tokens']) + int(row['output_tokens'])) / 1000, lambda n: f'{n:.1f}'),
        ('task-cost', 'Per-task estimated cost (off-peak)', 'CNY',
         lambda row: float(row['estimated_cny_off_peak']), lambda n: f'{n:.4f}'),
    ]
    for name, title, unit, value, formatter in charts:
        for theme in ['light', 'dark']:
            draw_chart(args.output_dir / f'{name}-{theme}', title, unit, value, formatter,
                       records, cases, regular, bold, theme)
    print('Rendered three transparent Times New Roman charts in light/dark variants:', args.output_dir)


if __name__ == '__main__':
    main()
