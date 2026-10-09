// Optional browser regression: Node 22+ and Chromium, no provider calls.
// node scripts/test_web_sessions.mjs /path/to/chromium
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile,mkdtemp,rm} from 'node:fs/promises';
import {spawn} from 'node:child_process';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {once} from 'node:events';
const browserPath=process.argv[2];
if(!browserPath)throw new Error('Usage: node scripts/test_web_sessions.mjs /path/to/chromium');
const snapshot={instance_id:'fixture',server_time:Date.now(),workspace:'/test',version:'test',commands:[],state:{session_id:'current',sequence:1,busy:false,starting:false,cells:[],metadata:{}}};
let sessions=[{id:'current',prompt_preview:'Current',updated_at:new Date().toISOString()},{id:'one',prompt_preview:'Old task <script>alert(1)</script>',model:'model-a',updated_at:'2026-01-02T12:00:00Z'},{id:'two',prompt_preview:'Other task',model:'model-b',updated_at:'2026-01-01T12:00:00Z'}];
const requests=[];let failDelete=true;
const streams=new Set();
const server=createServer(async(req,res)=>{
  try {
    if(req.url==='/api/events'){res.writeHead(200,{'Content-Type':'text/event-stream'});res.write(': connected\n\n');streams.add(res);req.on('close',()=>streams.delete(res));return;}
    let data;
    if(req.url==='/api/bootstrap')data=snapshot;
    else if(req.url==='/api/sessions')data={sessions};
    else if(req.url==='/api/sessions/delete'){
      const chunks=[];for await(const chunk of req)chunks.push(chunk);
      const body=JSON.parse(Buffer.concat(chunks));requests.push(body);
      if(failDelete){res.writeHead(409,{'Content-Type':'application/json'});res.end(JSON.stringify({error:'fixture conflict; retry'}));return;}
      sessions=sessions.filter(s=>!body.ids.includes(s.id));data={deleted:body.ids};
    }else {
      const file=req.url==='/'?'index.html':req.url.slice(1);
      if(!/^[\w.-]+$/.test(file)){res.writeHead(404);res.end();return;}
      res.writeHead(200,{'Content-Type':file.endsWith('.js')?'text/javascript':file.endsWith('.css')?'text/css':file.endsWith('.svg')?'image/svg+xml':'text/html'});
      res.end(await readFile(new URL(`../web/app/${file}`,import.meta.url)));return;
    }
    res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify(data));
  }catch(error){res.writeHead(500);res.end(String(error));}
});
const profile=await mkdtemp(join(tmpdir(),'latch-web-sessions-'));
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
  console.log('Session fixture listening');
  browser=spawn(browserPath,['--headless','--no-sandbox','--disable-gpu','--remote-debugging-port=0',`--user-data-dir=${profile}`,'about:blank'],{stdio:['ignore','ignore','pipe']});
  const endpoint=await new Promise((resolve,reject)=>{let log='';const timeout=setTimeout(()=>reject(new Error(`Chromium startup timed out: ${log}`)),10000);timeout.unref();browser.stderr.on('data',chunk=>{log+=chunk;const match=log.match(/DevTools listening on (ws:\/\/[^\s]+)/);if(match){clearTimeout(timeout);resolve(match[1]);}});browser.once('error',reject);browser.once('exit',code=>reject(new Error(`Chromium exited: ${code}\n${log}`)));});
  console.log('Chromium ready');
  const port=new URL(endpoint).port;
  const targets=await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  socket=new WebSocket(targets.find(t=>t.type==='page').webSocketDebuggerUrl);await once(socket,'open');
  socket.addEventListener('message',event=>{const message=JSON.parse(event.data);if(message.id){const task=pending.get(message.id);pending.delete(message.id);if(message.error)task.reject(new Error(JSON.stringify(message.error)));else task.resolve(message.result);}});
  await cdp('Page.navigate',{url:`http://127.0.0.1:${server.address().port}/`});
  await waitFor(`document.querySelectorAll('[data-session]').length===3`);
  await click('#manage-sessions');
  assert.equal(await evaluate(`document.querySelector('[data-select-session="current"]').disabled`),true);
  assert.equal(await evaluate(`document.querySelector('#sessions script')`),null);
  await click('[data-select-session="one"]');
  await evaluate(`{const input=document.querySelector('#session-search');input.value='model-b';input.dispatchEvent(new Event('input',{bubbles:true}));}`);
  await waitFor(`document.querySelectorAll('[data-session]').length===1`);
  await click('#select-visible-sessions');
  assert.equal(await evaluate(`document.querySelector('#delete-sessions').textContent`),'Delete selected (2)');
  await click('#delete-sessions');
  assert.equal(await evaluate(`document.querySelectorAll('#delete-session-names li').length`),2);
  assert.equal(await evaluate(`document.querySelector('#delete-session-names script')`),null);
  await click('[data-close="delete-sessions-dialog"]');
  assert.equal(requests.length,0);
  await click('#delete-sessions');await click('#confirm-delete-sessions');
  await waitFor(`document.querySelector('#delete-sessions-error').textContent.includes('fixture conflict')`);
  assert.equal(await evaluate(`document.querySelector('#delete-sessions-dialog').open`),true);
  assert.deepEqual(requests[0],{ids:['one','two'],instance_id:'fixture'});
  failDelete=false;await click('#confirm-delete-sessions');
  await waitFor(`!document.querySelector('#delete-sessions-dialog').open && document.querySelectorAll('[data-session]').length===0`);
  assert.equal(await evaluate(`document.querySelector('.session-empty').textContent`),'No matching conversations.');
  await evaluate(`{const input=document.querySelector('#session-search');input.value='';input.dispatchEvent(new Event('input',{bubbles:true}));}`);
  await waitFor(`document.querySelectorAll('[data-session]').length===1`);
  assert.equal(await evaluate(`document.querySelector('[data-session]').dataset.session`),'current');
  await cdp('Emulation.setDeviceMetricsOverride',{width:390,height:844,deviceScaleFactor:1,mobile:true});
  await click('#expand-sidebar');
  assert.equal(await evaluate(`document.documentElement.scrollWidth<=window.innerWidth`),true);
  console.log('Web session browser checks passed (search, selection across filters, confirmation, escaping, cancel, failure/retry, current protection, mobile).');
}finally{
  socket?.close();if(browser && browser.exitCode===null){const exited=once(browser,'exit').catch(()=>{});browser.kill('SIGKILL');await exited;}
  for(const stream of streams)stream.end();server.close();server.closeAllConnections();
  await rm(profile,{recursive:true,force:true});
}
