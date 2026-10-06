import http.server,threading,subprocess,json,pathlib,os,sqlite3,time,shutil
ROOT=pathlib.Path(__file__).resolve().parents[4]
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

def run(name,turns,setup=None):
 dest=OUT/name;dest.mkdir(mode=0o700);ws=dest/'workspace';ws.mkdir();(ws/'x.txt').write_text('original\n')
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
shell_timeout_seconds = 1
[safety]
level = "standard"
''');cfg.chmod(0o600)
 cmd=[str(ROOT/'target/release/latch'),'run','--config',str(cfg),'--workspace',str(ws),'--prompt','Investigate a deterministic isolated fixture.','--output','json']
 env=os.environ.copy();env['LATCH_INVESTIGATION_MOCK_KEY']='local-dummy-key'
 t=time.monotonic();p=subprocess.run(cmd,cwd=ws,env=env,text=True,capture_output=True,timeout=30);elapsed=round(time.monotonic()-t,3);server.shutdown();server.server_close()
 (dest/'stdout.json').write_text(p.stdout);(dest/'stderr.log').write_text(p.stderr)
 res=json.loads(p.stdout)
 c=sqlite3.connect(dest/'state/latch.sqlite3');events=[{'rowid':rid,'sequence':seq,'kind':k,'data':json.loads(v)['data']} for rid,seq,k,v in c.execute('select rowid,sequence,kind,payload from events order by rowid')];c.close()
 report={'name':name,'exit_code':p.returncode,'seconds':elapsed,'status':res['status'],'task':res['task'],'requests':server.index,'events':events,'final_x':(ws/'x.txt').read_text()}
 (dest/'repro.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
 print(name,'exit',p.returncode,'status',res['status'],'completion',res['task']['completion'],'x',repr(report['final_x']),flush=True)
 return report

run("validate_ignores_shell_timeout",[("validate",{"requirement":"bounded slow check","command":"python3 -B -c \"import time; time.sleep(2)\""}),("complete",{"implementation_done":True}),("TEXT","Done")])
run("shell_honors_timeout",[("shell",{"command":"python3 -B -c \"import time; time.sleep(2)\""}),("TEXT","Done")])
