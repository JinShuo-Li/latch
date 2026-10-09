#!/usr/bin/env python3
"""Summarize private threshold attempts without model calls or secret exports."""
import argparse
import collections
import datetime as dt
import hashlib
import json
import re
from pathlib import Path
import sqlite3

import run_thresholds


def analyze(attempt, source):
    result_path=attempt/'result.json'
    result=json.loads(result_path.read_text()) if result_path.exists() else {'threshold':int(attempt.name.split('-')[0]),'pending':True}
    result['attempt_id']=attempt.parent.name+'/'+attempt.name
    db=attempt/'state/latch.sqlite3'
    if not db.exists(): return result
    result.update(run_thresholds.metrics(db.resolve()))
    con=sqlite3.connect(db.resolve().as_uri()+'?mode=ro',uri=True)
    try: rows=list(con.execute('SELECT timestamp,kind,payload FROM events ORDER BY rowid'))
    finally: con.close()
    calls={};seen=set();repeats=collections.Counter();pre_edit=collections.Counter()
    edited=False;start=None;finish=None;peak_recent=0
    reasons=collections.Counter()
    for stamp,kind,payload in rows:
        data=json.loads(payload).get('data',{})
        if kind=='run_started' and start is None:start=stamp
        if kind=='run_completed':finish=stamp
        if kind=='file_changed':edited=True
        if kind=='context_materialized':peak_recent=max(peak_recent,data['stats'].get('recent_tokens',0))
        if kind=='context_epoch_started':reasons[data.get('reason','unknown')]+=1
        if kind=='tool_requested':calls[data['call']['id']]=data['call']
        if kind in ('tool_completed','tool_failed'):
            output=data['result'];call=calls.get(output['call_id'])
            if not call or call['name'] not in ('read_file','search','shell'):continue
            key=(call['name'],json.dumps(call['arguments'],sort_keys=True),hashlib.sha256(output.get('output','').encode()).hexdigest())
            if key in seen:
                repeats[call['name']]+=1
                if not edited:pre_edit[call['name']]+=1
            seen.add(key)
    result['durable_run_seconds']=round((dt.datetime.fromisoformat(finish)-dt.datetime.fromisoformat(start)).total_seconds(),3) if finish and start else None
    result['peak_recent_tokens']=peak_recent
    result['rotation_reasons']=dict(reasons)
    result['background_poll_calls']=result.get('tools',{}).get('exec_poll',0)
    result['full_suite_commands']=sum('pytest' in c['arguments'].get('command','') and bool(re.search(r'(?:^|\s)testing/?(?:\s|$)',c['arguments'].get('command',''))) for c in calls.values())
    result['repeats_by_tool']=dict(repeats)
    result['pre_edit_repeats_by_tool']=dict(pre_edit)
    workspace=attempt/'workspace'
    materials=[p for p in (source/'repo/.case').rglob('*') if p.is_file() and 'runs' not in p.relative_to(source/'repo/.case').parts and '__pycache__' not in p.parts]
    changed=[str(p.relative_to(source/'repo')) for p in materials if not (workspace/p.relative_to(source/'repo')).exists() or p.read_bytes()!=(workspace/p.relative_to(source/'repo')).read_bytes()]
    result['case_materials_unchanged']=not changed
    result['changed_case_materials']=changed
    if 'checks' in result and (attempt/'edges.json').exists():
        checks=result['checks']
        actual=sorted(set(checks['edge_actual']))
        checks['edge_actual']=actual
        edges=json.loads((attempt/'edges.json').read_text())
        checks['compatibility_passed']=edges.get('exit_code')==1 and actual==checks['edge_expected']
        result['passed']=result.get('exit_code')==0 and not changed and all(checks[k] for k in ['verify_passed','focused_passed','compatibility_passed'])
    usage=result.get('usage',{})
    input_total=usage.get('input_tokens',0);hit=usage.get('cache_read_tokens',0)
    result['cache_read_fraction']=round(hit/input_total,4) if input_total else None
    # Dated hypothetical DeepSeek direct API rates, not an OpenCode Go invoice.
    prices=json.loads((Path(__file__).parent/'reports/2026-10-06-full-comparison/deepseek-pricing.json').read_text())
    result['cost_scenarios']={currency:{period:round(((input_total-hit)*rates['input_miss']+hit*rates['input_hit']+usage.get('output_tokens',0)*rates['output'])/1e6,6) for period,rates in periods.items()} for currency,periods in prices['rates_per_million'].items()}
    if changed:result['passed']=False
    return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('roots',type=Path,nargs='+')
    parser.add_argument('--source',type=Path,required=True)
    parser.add_argument('--output',type=Path)
    args=parser.parse_args()
    runs=[analyze(a,args.source) for root in args.roots for a in sorted(root.glob('*-trial-*'))]
    report={'generated_at':dt.datetime.now(dt.timezone.utc).isoformat(),'runs':runs,
            'cost_note':'Hypothetical direct DeepSeek prices retrieved 2026-10-06 applied to observed Go usage; not actual billing.',
            'repeat_note':'Identical tool arguments and full result output. Repeated shell outputs may be necessary revalidation; pre-edit repeats are reported separately.'}
    text=json.dumps(report,ensure_ascii=False,indent=2)+'\n'
    if args.output:args.output.write_text(text)
    else:print(text,end='')


if __name__=='__main__':main()
