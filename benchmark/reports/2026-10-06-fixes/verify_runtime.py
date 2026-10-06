import http.server,threading,subprocess,json,pathlib,os,sqlite3,time,shutil
ROOT=pathlib.Path(__file__).resolve().parents[3]
OUT=pathlib.Path(os.environ.get('LATCH_INVESTIGATION_OUT', str(ROOT/'benchmark/runs'/('investigation-'+str(time.time_ns())))))
OUT.mkdir(mode=0o700,exist_ok=True)
class Handler(http.server.BaseHTTPRequestHandler):
 def do_POST(self):
  self.rfile.read(int(self.headers.get('Content-Length','0')))
  i=self.server.index;self.server.index+=1
  name,args=self.server.turns[min(i,len(self.server.turns)-1)]
  if name=='TEXT':delta={'content':args};reason='stop'
  else:delta={'tool_calls':[{'index':0,'id':'call_'+str(i),'type':'function','function':{'name':name,'arguments':json.dumps(args)}}]};reason='tool_calls'
  frames=[{'choices':[{'delta':delta,'finish_reason':None}]},{'choices':[{'delta':{},'finish_reason':reason}]}]
  body=''.join('data: '+json.dumps(f)+'\n\n' for f in frames)+'data: [DONE]\n\n'
  b=body.encode();self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b)
 def log_message(self,*a):pass

def run(name,turns,setup=None,timeout_seconds=120):
 dest=OUT/name;dest.mkdir(mode=0o700);ws=dest/'workspace';ws.mkdir();(ws/'x.txt').write_text('original\n')
 for args in [['init','-q'],['add','x.txt'],['-c','user.name=Fixture','-c','user.email=fixture@localhost','commit','-qm','baseline']]:
  subprocess.run(['git',*args],cwd=ws,check=True,capture_output=True)
 if setup:setup(ws)
 server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);server.turns=turns;server.index=0
 threading.Thread(target=server.serve_forever,daemon=True).start()
 cfg=dest/'config.toml';cfg.write_text(f'''state_dir = {json.dumps(str(dest/'state'))}
default_mode = "WORK"
[providers.mock]
kind = "openai-compatible"
base_url = "http://127.0.0.1:{server.server_port}/v1"
credential = "env:LATCH_INVESTIGATION_MOCK_KEY"
default_model = "mock-model"
[inference]
provider = "mock"
model = "mock-model"
[permissions]
mode = "human"
shell_timeout_seconds = {timeout_seconds}
[safety]
level = "standard"
''');cfg.chmod(0o600)
 cmd=[str(ROOT/'target/release/latch'),'run','--config',str(cfg),'--workspace',str(ws),'--prompt','Investigate a deterministic isolated fixture.','--output','json']
 env=os.environ.copy();env['LATCH_INVESTIGATION_MOCK_KEY']='local-dummy-key'
 t=time.monotonic();p=subprocess.run(cmd,cwd=ws,env=env,text=True,capture_output=True,timeout=30);elapsed=round(time.monotonic()-t,3);server.shutdown();server.server_close()
 (dest/'stdout.json').write_text(p.stdout);(dest/'stderr.log').write_text(p.stderr)
 res=json.loads(p.stdout)
 c=sqlite3.connect(dest/'state/latch.sqlite3');events=[{'rowid':rid,'sequence':seq,'kind':k,'data':json.loads(v)['data']} for rid,seq,k,v in c.execute('select rowid,sequence,kind,payload from events order by rowid')];c.close()
 report={'name':name,'exit_code':p.returncode,'seconds':elapsed,'status':res['status'],'task':res['task'],'requests':server.index,'events':events,'final_x':(ws/'x.txt').read_text(),'text':res['result']['text']}
 (dest/'repro.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
 print(name,'exit',p.returncode,'status',res['status'],'completion',res['task']['completion'],'x',repr(report['final_x']),flush=True)
 return report

complete=('complete',{'implementation_done':True})
text=('TEXT','Done')
reports=[]
def check(name,turns,completion,setup=None,timeout_seconds=120):
 r=run(name,turns,setup,timeout_seconds)
 assert r['task']['completion']==completion,r
 reports.append(r)
 return r
r=check('validation_set',[
 ('validate',{'requirement':'A','command':'python3 -B -c "assert True"'}),
 ('validate',{'requirement':'B','command':'python3 -B -c "assert True"'}),
 ('validate',{'requirement':'A','requirements':['B'],'command':'python3 -B -c "assert True"'}),complete], 'verified')
assert r['task']['validation']['passed']==2,r
r=check('pipeline_failure',[
 ('validate',{'requirement':'check','command':'python3 -B -c "assert False" 2>&1 | tail -3'}),complete,text], 'implemented_not_verified')
assert r['task']['validation']['failed']==1,r
r=check('missing_completion_claim',[
 ('shell',{'command':'printf changed > x.txt'}),
 ('validate',{'requirement':'check','command':'python3 -B -c "assert True"'}),text,complete], 'verified')
assert r['requests']==4,r
r=check('readonly_filter',[
 ('shell',{'command':'ls -la && find . -type f -name "*.py" -o -name "*.md" -o -name "*.toml" -o -name "*.cfg" | grep -v ".git/" | head -50'}),text], 'in_progress')
assert r['exit_code']==0,r

def git_setup(ws):
 (ws/'input.txt').write_text('before\n')
 helper=ws/'diff_helper.py';helper.write_text('from pathlib import Path\nPath("x.txt").write_text("unauthorized")\n')
 for args in [['init','-q'],['add','-A'],['-c','user.name=Fixture','-c','user.email=fixture@localhost','commit','-qm','baseline'],['config','diff.external','python3 '+str(helper)]]:
  subprocess.run(['git',*args],cwd=ws,check=True,capture_output=True)
 (ws/'input.txt').write_text('after\n')
r=check('readonly_git_external_diff',[
 ('validate',{'requirement':'check','command':'python3 -B -c "assert True"'}),
 ('shell',{'command':'git diff --ext-diff'}),complete], 'verified',git_setup)
assert r['final_x']=='original\n',r
assert any(e['kind']=='tool_failed' for e in r['events']),r
r=check('configured_validation_timeout',[
 ('validate',{'requirement':'check','command':'python3 -B -c "import time; time.sleep(2)"'}),complete,text], 'implemented_not_verified',timeout_seconds=1)
assert r['task']['validation']['passed']==0 and r['seconds']<2,r
assert any(e['kind']=='tool_failed' and 'timed out' in e['data']['result']['output'] for e in r['events']),r
r=check('empty_completion_summary',[
 ('shell',{'command':'printf changed > x.txt'}),
 ('validate',{'requirement':'check','command':'python3 -B -c "assert True"'}),complete], 'verified')
assert r['requests']==3 and 'Kernel completion report' in r['text'] and 'x.txt' in r['text'],r
summary=[{k:r[k] for k in ['name','exit_code','seconds','status','task','requests','final_x','text']} for r in reports]
(OUT/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print('ALL SEVEN REGRESSIONS PASSED',flush=True)
