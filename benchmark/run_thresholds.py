#!/usr/bin/env python3
"""Opt-in paid pytest #14998 working-history calibration; isolated attempts."""
import argparse
import collections
import concurrent.futures
import datetime as dt
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import signal
import sqlite3
import subprocess
import tarfile
import time

BASELINE = "3fd8675d6d798507c06cf9c60753be6d9d7b0e17"
ROOT = Path(__file__).resolve().parent


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def private_config(source, destination, state, threshold):
    raw = source.read_text()
    raw, count = re.subn(r"(?m)^state_dir\s*=.*$", "state_dir = " + json.dumps(str(state)), raw)
    if not count:
        raw = "state_dir = " + json.dumps(str(state)) + "\n" + raw
    raw, count = re.subn(r"(?m)^recent_tokens\s*=\s*\d+.*$", f"recent_tokens = {threshold}", raw)
    if not count:
        raw, count = re.subn(r"(?m)^\[context\]\s*$", f"[context]\nrecent_tokens = {threshold}", raw)
        if not count:
            raw += f"\n[context]\nrecent_tokens = {threshold}\n"
    destination.write_text(raw)
    destination.chmod(0o600)


def metrics(database):
    if not database.exists():
        return {}
    con = sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=1)
    try:
        rows = list(con.execute("SELECT sequence,timestamp,kind,payload FROM events ORDER BY rowid"))
    finally:
        con.close()
    counts = collections.Counter()
    usage = collections.Counter()
    tools = collections.Counter()
    calls, results = {}, {}
    rotations, repeats = 0, 0
    seen = set()
    first_edit = None
    start = None
    peak = 0
    completion = None
    for seq, stamp, kind, payload in rows:
        d = json.loads(payload).get("data", {})
        counts[kind] += 1
        if kind == "run_started" and start is None:
            start = dt.datetime.fromisoformat(stamp)
        if kind == "file_changed" and first_edit is None and start:
            first_edit = round((dt.datetime.fromisoformat(stamp)-start).total_seconds(),3)
        if kind == "context_epoch_started" and "high-water" in d.get("reason", ""):
            rotations += 1
        if kind == "context_materialized":
            peak = max(peak,d["stats"].get("total_tokens",0))
        if kind == "model_usage":
            for k,v in d["usage"].items():
                if isinstance(v,int): usage[k] += v
        if kind == "tool_requested":
            call=d["call"];calls[call["id"]]=call;tools[call["name"]]+=1
        if kind in ("tool_completed","tool_failed"):
            result=d["result"];results[result["call_id"]]=result
        if kind == "completion_changed":
            completion=d.get("completion")
    repeat_by_tool=collections.Counter()
    for key,call in calls.items():
        if call["name"] not in ("read_file","search","shell") or key not in results:
            continue
        output=results[key].get("output", "")
        fingerprint=(call["name"],json.dumps(call["arguments"],sort_keys=True),hashlib.sha256(output.encode()).hexdigest())
        if fingerprint in seen:
            repeats+=1;repeat_by_tool[call["name"]]+=1
        seen.add(fingerprint)
    return {"model_requests":counts["model_request_started"],"tool_calls":counts["tool_requested"],
            "first_edit_seconds":first_edit,"rotations":rotations,"epochs":counts["context_epoch_started"],
            "exact_repeated_observations":repeats,"repeats_by_tool":dict(repeat_by_tool),
            "tools":dict(tools),"usage":dict(usage),"peak_request_tokens":peak,"completion":completion}


def boundary(workspace, command, env, timeout=180):
    # Hide the home tree and remount only the fixture and Python runtime.
    python_root=(workspace/'.venv/bin/python').resolve().parent.parent
    args=[shutil.which('bwrap'),'--die-with-parent','--new-session','--unshare-user','--unshare-pid',
          '--unshare-ipc','--unshare-uts','--unshare-cgroup-try','--unshare-net','--ro-bind','/','/',
          '--dev','/dev','--proc','/proc','--tmpfs','/tmp','--tmpfs','/run','--tmpfs','/home',
          '--ro-bind',str(python_root),str(python_root),'--bind',str(workspace),str(workspace),
          '--chdir',str(workspace),'--',*command]
    try:
        result=subprocess.run(args,env=env,text=True,capture_output=True,timeout=timeout)
        return {"exit_code":result.returncode,"stdout":result.stdout,"stderr":result.stderr}
    except subprocess.TimeoutExpired:
        return {"exit_code":None,"timed_out":True}


def prepare(source, attempt, threshold):
    attempt.mkdir(mode=0o700)
    workspace=attempt/'workspace';workspace.mkdir()
    archive=subprocess.check_output(['git','archive',BASELINE],cwd=source/'repo')
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        tar.extractall(workspace,filter='data')
    shutil.copytree(source/'repo/.case',workspace/'.case',ignore=shutil.ignore_patterns('runs','__pycache__'))
    shutil.copytree(source/'repo/.venv',workspace/'.venv',symlinks=True,ignore=shutil.ignore_patterns('__pycache__','*.pyc'))
    for path in (workspace/'.venv/lib').glob('python*/site-packages/*.pth'):
        text=path.read_text()
        if str(source/'repo') in text:
            path.write_text(text.replace(str(source/'repo'),str(workspace)))
    for path in (workspace/'.venv/bin').iterdir():
        if not path.is_file() or path.is_symlink(): continue
        try: text=path.read_text()
        except UnicodeError: continue
        fixed=re.sub(r'(?m)^#![^\n]*?/.venv/bin/python[^\n]*',f'#!{workspace}/.venv/bin/python',text)
        if fixed!=text: path.write_text(fixed)
    generated=source/'repo/src/_pytest/_version.py'
    if generated.exists(): shutil.copy2(generated,workspace/'src/_pytest/_version.py')
    ignore=workspace/'.gitignore'
    with ignore.open('a') as stream:stream.write('\n.venv/\n.case/\n.calibration/\n')
    for args in [['init','-q'],['add','-A'],['-c','user.name=Latch Calibration','-c','user.email=calibration@localhost','commit','-qm','baseline']]:
        subprocess.run(['git',*args],cwd=workspace,check=True,capture_output=True)
    private_config(source/'config.toml',attempt/'config.toml',attempt/'state',threshold)
    shutil.copy2(source/'PROMPT.md',attempt/'PROMPT.md')
    env=dict(os.environ,PYTEST_DISABLE_PLUGIN_AUTOLOAD='1',PYTHONDONTWRITEBYTECODE='1',VIRTUAL_ENV=str(workspace/'.venv'))
    env['PATH']=str(workspace/'.venv/bin')+os.pathsep+env['PATH']
    baseline=boundary(workspace,[str(workspace/'.venv/bin/python'),'.case/verify.py'],env)
    save(attempt/'baseline.json',baseline)
    if baseline.get('exit_code') != 1 or 'setup_error' not in baseline.get('stdout',''):
        raise RuntimeError('baseline did not demonstrate the expected retention bug')
    return workspace,env


def acceptance(source,attempt,workspace,env):
    python=str(workspace/'.venv/bin/python')
    verify=boundary(workspace,[python,'.case/verify.py'],env)
    focused=boundary(workspace,[python,'-m','pytest','-q','testing/test_tmpdir.py','testing/test_runner.py','testing/test_skipping.py'],env,240)
    audit=workspace/'.calibration';audit.mkdir(exist_ok=True)
    shutil.copy2(source/'audit/test_edges.py',audit/'test_edges.py')
    (audit/'pytest.ini').write_text('[pytest]\ntmp_path_retention_policy=failed\n')
    edges=boundary(workspace,[python,'-m','pytest','-q','-c',str(audit/'pytest.ini'),'--confcutdir='+str(audit),'--basetemp='+str(audit/'retained'),str(audit/'test_edges.py')],env)
    actual=sorted({p.stem for p in (audit/'retained').rglob('*.evidence')})
    expected=sorted(['early','late','skip_error','legacy_setup','legacy_teardown','multiple_errors','call_skip'])
    for name,result in [('verify',verify),('focused',focused),('edges',edges)]:
        save(attempt/(name+'.json'),result)
    return {"verify_passed":verify.get('exit_code')==0,"focused_passed":focused.get('exit_code')==0,
            "compatibility_passed":edges.get('exit_code')==1 and actual==expected,
            "edge_expected":expected,"edge_actual":actual}


def run_one(args,root,threshold,trial):
    attempt=root/f'{threshold}-trial-{trial}'
    workspace,env=prepare(args.source,attempt,threshold)
    print(json.dumps({"started":attempt.name,"threshold":threshold}),flush=True)
    command=[str(args.latch),'run','--workspace',str(workspace),'--config',str(attempt/'config.toml'),
             '--prompt-file',str(attempt/'PROMPT.md'),'--output','jsonl']
    started=time.monotonic();stop_reason=None
    with (attempt/'cli.jsonl').open('w') as out,(attempt/'cli.stderr.log').open('w') as err:
        process=subprocess.Popen(command,cwd=workspace,env=env,stdout=out,stderr=err,start_new_session=True)
        next_progress=0
        while process.poll() is None:
            try:
                process.wait(timeout=1)
                break
            except subprocess.TimeoutExpired:
                pass
            if time.monotonic()<next_progress: continue
            next_progress=time.monotonic()+30
            try:m=metrics(attempt/'state/latch.sqlite3')
            except sqlite3.Error:m={}
            elapsed=round(time.monotonic()-started,1)
            print(json.dumps({"progress":attempt.name,"elapsed_seconds":elapsed,**m}),flush=True)
            usage=m.get('usage',{})
            if elapsed>args.timeout:stop_reason='time limit'
            elif usage.get('input_tokens',0)>args.input_limit:stop_reason='input token limit'
            elif usage.get('output_tokens',0)>args.output_limit:stop_reason='output token limit'
            if stop_reason:
                os.killpg(process.pid,signal.SIGINT)
                try:process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid,signal.SIGKILL);process.wait()
                break
    wall=round(time.monotonic()-started,3)
    m=metrics(attempt/'state/latch.sqlite3')
    patch=subprocess.check_output(['git','diff','--binary','HEAD'],cwd=workspace,text=True)
    (attempt/'product.patch').write_text(patch)
    untracked=subprocess.check_output(['git','ls-files','--others','--exclude-standard'],cwd=workspace,text=True)
    (attempt/'untracked-files.txt').write_text(untracked)
    checks=acceptance(args.source,attempt,workspace,env)
    report={"threshold":threshold,"trial":trial,"wall_seconds":wall,"exit_code":process.returncode,
            "stop_reason":stop_reason,**m,"checks":checks,
            "passed":process.returncode==0 and all(checks[k] for k in ['verify_passed','focused_passed','compatibility_passed'])}
    save(attempt/'result.json',report)
    print(json.dumps({"finished":attempt.name,**report}),flush=True)
    return report


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source',type=Path,required=True,help='original local pytest #14998 experiment directory')
    parser.add_argument('--latch',type=Path,default=ROOT.parent/'target/release/latch')
    parser.add_argument('--thresholds',default='32000,64000,96000,128000')
    parser.add_argument('--trials',type=int,default=1)
    parser.add_argument('--jobs',type=int,choices=[1,2,3],default=2)
    parser.add_argument('--timeout',type=int,default=1800)
    parser.add_argument('--input-limit',type=int,default=5000000)
    parser.add_argument('--output-limit',type=int,default=150000)
    parser.add_argument('--live',action='store_true',help='authorize real paid provider calls')
    args=parser.parse_args()
    thresholds=[int(x) for x in args.thresholds.split(',')]
    if shutil.which('bwrap') is None:parser.error('Bubblewrap is required for isolated execution')
    if not args.live:parser.error('--live is required; this experiment uses a paid model')
    if not all(x>0 for x in thresholds) or args.trials<1:parser.error('positive thresholds and trials required')
    root=ROOT/'runs'/('thresholds-'+dt.datetime.now(dt.timezone.utc).strftime('%Y%m%dT%H%M%SZ'))
    root.mkdir(mode=0o700,parents=True)
    manifest={"baseline":BASELINE,"latch_commit":subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
              "binary_sha256":hashlib.sha256(args.latch.read_bytes()).hexdigest(),"thresholds":thresholds,"trials":args.trials,
              "runner_sha256":hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "jobs":args.jobs,"timeout":args.timeout,"input_limit":args.input_limit,"output_limit":args.output_limit}
    save(root/'manifest.json',manifest)
    print(json.dumps({"run_root":str(root),**manifest}),flush=True)
    reports=[]
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures=[pool.submit(run_one,args,root,t,n) for n in range(1,args.trials+1) for t in thresholds]
        for future in concurrent.futures.as_completed(futures):
            try:reports.append(future.result())
            except Exception as error:print(json.dumps({"error":str(error)}),flush=True)
            save(root/'summary.json',reports)
    if len(reports)!=len(futures):raise SystemExit(1)


if __name__=='__main__':main()
