// Optional browser regression check: Node 22+ and a Chromium executable.
// node scripts/test_web_setup.mjs /path/to/chromium
// Serves production assets with an isolated transport fixture; no provider calls.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile, mkdtemp, rm} from 'node:fs/promises';
import {spawn} from 'node:child_process';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {once} from 'node:events';

const browserPath=process.argv[2];
if(!browserPath)throw new Error('Usage: node scripts/test_web_setup.mjs /path/to/chromium');
const model=(id)=>({id,display_name:id,efforts:['low','high'],default_effort:'low',input_modalities:['text','image']});
const snapshot={instance_id:'test',server_time:Date.now(),workspace:'/test/workspace',version:'test',commands:[],state:{session_id:'test-session',sequence:1,busy:false,starting:false,cells:[],metadata:{setup_required:true,setup_paths:{config_exists:false,config_path:'/test/.latch/config.toml',state_root:'/test/.latch'},setup_catalog:[{kind:'opencode-go',label:'OpenCode Go',default_base_url:'https://example.test/v1',requires_base_url:false,credential_label:'env:OPENCODE_GO_API_KEY',default_model:'model-a',models:[model('model-a'),model('model-b')]}],setup_providers:[]}}};
const requests=[];
let failSave=true;
let holdSave=false;
let finishSave;
const streams=new Set();
const emit=()=>{snapshot.state.sequence++;for(const stream of streams)stream.write('event: changed\ndata: {}\n\n');};
const provider=(name)=>({id:name,display_name:name,kind:'opencode-go',status:'Ready',credential_ref:'secret:test',base_url:'https://example.test/v1',default_model:'model-a',model_count:2,available_models:['model-a','model-b'],models:['model-a','model-b'].map(id=>({...model(id),enabled:true,resolved:true,transport:'chat_completions',context_window_tokens:100000,aliases:[],effort_map:{}}))});
// A synthesized legacy fallback must never appear as a configured account.
snapshot.state.metadata.setup_providers=[provider('openai')];
snapshot.state.metadata.header={model:'display-only-openai-model'};
const server=createServer(async(req,res)=>{
  try {
    if(req.url==='/api/events'){res.writeHead(200,{'Content-Type':'text/event-stream'});res.write(': connected\n\n');streams.add(res);req.on('close',()=>streams.delete(res));return;}
    let data;
    if(req.url==='/api/bootstrap')data=snapshot;
    else if(req.url==='/api/sessions')data={sessions:[]};
    else if(req.url.endsWith('/commands')){
      const chunks=[];for await(const chunk of req)chunks.push(chunk);
      const command=JSON.parse(Buffer.concat(chunks));requests.push(command.input);
      res.writeHead(200,{'Content-Type':'application/json'});res.end('{}');
      const complete=()=>{
        if(command.input.type==='setup_apply'){
          if(failSave){snapshot.state.cells.push({Error:{text:'error: invalid credential'}});failSave=false;}
          else {
            const plan=command.input.data;
            if(plan.Apply){snapshot.state.metadata.setup_providers=[provider(plan.Apply.name)];delete snapshot.state.metadata.setup_required;}
            if(plan.SetProviderField)snapshot.state.metadata.setup_providers[0].display_name=plan.SetProviderField.field.DisplayName;
            snapshot.state.cells.push({Notice:{text:'Configuration saved'}});
          }
        } else if(command.input.type==='discover_models')snapshot.state.metadata.setup_models={provider:'opencode-go',ids:['discovered-model']};
        emit();
      };
      if(holdSave)finishSave=complete;else setTimeout(complete,80);
      return;
    } else {
      const file=req.url==='/'?'index.html':req.url.slice(1);
      if(!/^[\w.-]+$/.test(file)){res.writeHead(404);res.end();return;}
      res.writeHead(200,{'Content-Type':file.endsWith('.js')?'text/javascript':file.endsWith('.css')?'text/css':file.endsWith('.svg')?'image/svg+xml':'text/html'});
      res.end(await readFile(new URL(`../web/app/${file}`,import.meta.url)));return;
    }
    res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify(data));
  }catch(error){res.writeHead(500);res.end(String(error));}
});
const profile=await mkdtemp(join(tmpdir(),'latch-web-setup-'));
let browser;
let socket;
const pending=new Map();
let nextId=0;
const cdp=(method,params={})=>new Promise((resolve,reject)=>{const id=++nextId;pending.set(id,{resolve,reject});socket.send(JSON.stringify({id,method,params}));});
const evaluate=async(expression)=>{const result=await cdp('Runtime.evaluate',{expression,awaitPromise:true,returnByValue:true});if(result.exceptionDetails)throw new Error(JSON.stringify(result.exceptionDetails));return result.result.value;};
const waitFor=async(expression)=>{const deadline=Date.now()+8000;while(!await evaluate(expression)){if(Date.now()>deadline)throw new Error(`Timed out: ${expression}`);await new Promise(r=>setTimeout(r,30));}};
const click=selector=>evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
const fill=(name,value)=>evaluate(`document.querySelector('[name="${name}"]').value=${JSON.stringify(value)}`);
const change=(name,value)=>evaluate(`{const el=document.querySelector('[name="${name}"]');el.value=${JSON.stringify(value)};el.dispatchEvent(new Event('change',{bubbles:true}));}`);
try {
  server.listen(0,'127.0.0.1');await once(server,'listening');
  console.log('Setup fixture listening');
  browser=spawn(browserPath,['--headless','--no-sandbox','--disable-gpu','--remote-debugging-port=0',`--user-data-dir=${profile}`,'about:blank'],{stdio:['ignore','ignore','pipe']});
  const endpoint=await new Promise((resolve,reject)=>{let log='';const timeout=setTimeout(()=>reject(new Error(`Chromium startup timed out: ${log}`)),10000);timeout.unref();browser.stderr.on('data',chunk=>{log+=chunk;const match=log.match(/DevTools listening on (ws:\/\/[^\s]+)/);if(match){clearTimeout(timeout);resolve(match[1]);}});browser.once('error',reject);browser.once('exit',code=>reject(new Error(`Chromium exited: ${code}\n${log}`)));});
  console.log('Chromium ready');
  const port=new URL(endpoint).port;
  const targets=await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  socket=new WebSocket(targets.find(t=>t.type==='page').webSocketDebuggerUrl);await once(socket,'open');
  socket.addEventListener('message',event=>{const message=JSON.parse(event.data);if(message.id){const task=pending.get(message.id);pending.delete(message.id);if(message.error)task.reject(new Error(JSON.stringify(message.error)));else task.resolve(message.result);}});
  await cdp('Page.navigate',{url:`http://127.0.0.1:${server.address().port}/`});
  await waitFor(`document.querySelector('#settings-dialog')?.open && !!document.querySelector('[data-settings-form="new-provider"]')`);
  assert.equal(await evaluate(`document.querySelector('[data-open-provider="openai"]')`),null);
  assert.equal(await evaluate(`document.querySelector('[name="credential"]').type`),'password');
  assert.equal(await evaluate(`document.querySelector('#send-button').disabled`),true);
  assert.equal(await evaluate(`document.querySelector('#model-label').textContent`),'Set up a provider');
  await change('source','Env');
  assert.equal(await evaluate(`document.querySelector('[name="credential"]').value`),'OPENCODE_GO_API_KEY');
  assert.equal(await evaluate(`document.querySelector('[name="credential"]').type`),'text');
  await change('source','Secret');await fill('credential','fixture-key');
  await change('effort','high');await click('[name="enabled_models"][value="model-b"]');
  await click('[data-settings-form="new-provider"] [type="submit"]');
  await waitFor(`document.querySelector('#settings-status').textContent.includes('invalid credential')`);
  assert.equal(await evaluate(`document.querySelector('[name="credential"]').value`),'fixture-key');
  assert.equal(await evaluate(`document.querySelector('#settings-status').getAttribute('role')`),'alert');
  holdSave=true;
  await click('[data-settings-form="new-provider"] [type="submit"]');
  await waitFor(`document.querySelector('#settings-status').textContent.includes('Saving')`);
  assert.equal(await evaluate(`document.querySelector('[data-settings-form="new-provider"] [type="submit"]').disabled`),true);
  await waitFor(`document.querySelector('[data-settings-form="new-provider"] [type="submit"]').disabled`);
  await new Promise(r=>setTimeout(r,100));finishSave();holdSave=false;
  await waitFor(`!!document.querySelector('[data-settings-form="credential"]')`);
  assert.equal(requests[1].data.Apply.effort,'high');
  assert.deepEqual(requests[1].data.Apply.enabled_models,['model-a','model-b']);
  assert.equal(await evaluate(`document.querySelector('[name="credential"]').value`),'');
  await change('field','DisplayName');await fill('value','Real account');await click('[data-settings-form="provider-field"] [type="submit"]');
  await waitFor(`document.querySelector('#settings-content h3').textContent==='Real account'`);
  await click('[data-discover]');await waitFor(`!!document.querySelector('[data-edit-model="discovered-model"]')`);
  await change('source','Env');
  assert.equal(await evaluate(`document.querySelector('[name="credential"]').value`),'OPENCODE_GO_API_KEY');
  await change('source','Secret');
  const beforeRemoval=requests.length;
  await click('[data-remove-provider]');
  assert.equal(await evaluate(`document.querySelector('#provider-removal').hidden`),false);
  assert.equal(requests.length,beforeRemoval);
  await click('[data-cancel-remove]');
  // Unrelated snapshots must preserve an unsaved form.
  await fill('credential','unsaved-key');snapshot.state.metadata.safety='standard';emit();
  await new Promise(r=>setTimeout(r,200));
  assert.equal(await evaluate(`document.querySelector('[name="credential"]').value`),'unsaved-key');
  // Configured startup does not reopen onboarding, including on mobile.
  await cdp('Emulation.setDeviceMetricsOverride',{width:390,height:844,deviceScaleFactor:1,mobile:true});
  await cdp('Page.reload');await waitFor(`document.querySelector('#connection-state').textContent==='CONNECTED'`);
  assert.equal(await evaluate(`document.querySelector('#settings-dialog').open`),false);
  await click('#open-settings');await click('[data-settings="providers"]');await click('[data-open-provider="__new"]');
  assert.equal(await evaluate(`document.querySelector('#settings-dialog').scrollWidth<=document.querySelector('#settings-dialog').clientWidth`),true);
  assert.equal(await evaluate(`document.querySelector('[name="name"]').value`),'opencode-go-2');
  await change('catalog_model','__custom');await fill('model','private-model');await change('transport','responses');await fill('credential','custom-key');
  await click('[data-settings-form="new-provider"] [type="submit"]');
  await waitFor(`!!document.querySelector('[data-settings-form="credential"]')`);
  const custom=requests.at(-1).data.Apply;
  assert.equal(custom.custom_transport,'responses');assert.deepEqual(custom.enabled_models,['private-model']);
  console.log('Web setup browser checks passed (onboarding, credentials, retry, pending save, live updates, drafts, custom models, configured restart, mobile).');
}finally{
  socket?.close();if(browser && browser.exitCode===null){const exited=once(browser,'exit').catch(()=>{});browser.kill('SIGKILL');await exited;}
  for(const stream of streams)stream.end();server.close();server.closeAllConnections();
  await rm(profile,{recursive:true,force:true});
}
