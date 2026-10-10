// Small CDP driver shared by optional website checks and real Web captures.
import {spawn} from 'node:child_process';
import {mkdtemp, rm, writeFile} from 'node:fs/promises';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {once} from 'node:events';

export async function openBrowser(executable) {
  const profile = await mkdtemp(join(tmpdir(), 'latch-site-browser-'));
  const browser = spawn(executable, ['--headless', '--no-sandbox', '--disable-gpu', '--remote-debugging-port=0', `--user-data-dir=${profile}`, 'about:blank'], {stdio:['ignore','ignore','pipe']});
  let socket;
  async function close() {
    socket?.close();
    if (browser.exitCode === null) { browser.kill(); await once(browser, 'exit'); }
    await rm(profile, {recursive:true, force:true});
  }
  try {
    const endpoint = await new Promise((resolve,reject) => {
      let log='';
      const timeout=setTimeout(()=>reject(new Error(`Chromium startup timed out: ${log}`)),10000);
      timeout.unref();
      browser.stderr.on('data', chunk=>{log+=chunk; const match=log.match(/DevTools listening on (ws:\/\/[^\s]+)/); if(match){clearTimeout(timeout);resolve(match[1]);}});
      browser.once('error',reject);
      browser.once('exit',code=>reject(new Error(`Chromium exited: ${code}\n${log}`)));
    });
    const targets = await (await fetch(`http://127.0.0.1:${new URL(endpoint).port}/json/list`)).json();
    socket = new WebSocket(targets.find(target=>target.type==='page').webSocketDebuggerUrl);
    await once(socket,'open');
    const pending=new Map(); let next=0;
    socket.addEventListener('message', event=>{const message=JSON.parse(event.data);if(message.id){const task=pending.get(message.id);pending.delete(message.id);if(message.error)task.reject(new Error(JSON.stringify(message.error)));else task.resolve(message.result);}});
    const cdp=(method,params={})=>new Promise((resolve,reject)=>{const id=++next;pending.set(id,{resolve,reject});socket.send(JSON.stringify({id,method,params}));});
    const evaluate=async(expression)=>{const result=await cdp('Runtime.evaluate',{expression,awaitPromise:true,returnByValue:true});if(result.exceptionDetails)throw new Error(JSON.stringify(result.exceptionDetails));return result.result.value;};
    const waitFor=async(expression)=>{const deadline=Date.now()+10000;while(!await evaluate(expression)){if(Date.now()>deadline)throw new Error(`Timed out: ${expression}`);await new Promise(resolve=>setTimeout(resolve,50));}};
    const screenshot=async(path)=>{await evaluate('document.fonts.ready.then(()=>true)');const result=await cdp('Page.captureScreenshot',{format:'png',captureBeyondViewport:false});await writeFile(path,Buffer.from(result.data,'base64'));};
    await cdp('Page.enable');
    return {cdp,evaluate,waitFor,screenshot,close};
  } catch(error) {await close();throw error;}
}
