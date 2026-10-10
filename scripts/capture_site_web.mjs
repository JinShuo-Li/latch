// Capture the actual local Web UI in isolated first-run setup. No provider calls.
import {spawn} from 'node:child_process';
import {mkdtemp, mkdir, rm} from 'node:fs/promises';
import {resolve, join} from 'node:path';
import {tmpdir} from 'node:os';
import {once} from 'node:events';
import {openBrowser} from './site_browser.mjs';
const [chromium, binary] = process.argv.slice(2);
if(!chromium || !binary) throw new Error('Usage: node scripts/capture_site_web.mjs <chromium> <latch-binary>');
const fixture=await mkdtemp(join(tmpdir(),'latch-site-capture-'));
await mkdir(join(fixture,'workspace'));
const app=spawn(resolve(binary),['--web','--ssh','16066'],{cwd:join(fixture,'workspace'),env:{...process.env,HOME:fixture,XDG_CONFIG_HOME:join(fixture,'config'),XDG_STATE_HOME:join(fixture,'state')},stdio:['ignore','pipe','pipe']});
let browser;
try {
  const url=await new Promise((resolve,reject)=>{
    let output='';const timeout=setTimeout(()=>reject(new Error('Web startup timeout: '+output)),15000);timeout.unref();
    const capture=chunk=>{output+=chunk;const match=output.match(/http:\/\/(?:127\.0\.0\.1|localhost):16066[^\s]*/);if(match){clearTimeout(timeout);resolve(match[0]);}};
    app.stdout.on('data',capture);app.stderr.on('data',capture);app.once('error',reject);app.once('exit',code=>reject(new Error('Latch exited '+code+': '+output)));
  });
  browser=await openBrowser(chromium);
  await browser.cdp('Emulation.setDeviceMetricsOverride',{width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await browser.cdp('Page.navigate',{url});
  await browser.waitFor(`document.querySelector('#settings-dialog')?.open && !!document.querySelector('[data-settings-form="new-provider"]')`);
  await browser.evaluate(`document.querySelector('[data-close="settings-dialog"]').click()`);
  await browser.waitFor(`!document.querySelector('#settings-dialog').open`);
  await browser.screenshot('site/assets/web.png');
  console.log('Captured production Web first-run workspace; no credential or provider call.');
} finally {
  await browser?.close();
  if(app.exitCode===null){app.kill('SIGINT');await once(app,'exit');}
  await rm(fixture,{recursive:true,force:true});
}
