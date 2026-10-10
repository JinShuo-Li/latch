// Optional browser checks for the built Pages artifact, served at a project subpath.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile, mkdir} from 'node:fs/promises';
import {resolve, extname} from 'node:path';
import {once} from 'node:events';
import {openBrowser} from './site_browser.mjs';
const [chromium, directory='target/site-preview'] = process.argv.slice(2);
if(!chromium) throw new Error('Usage: node scripts/test_site.mjs <chromium> [built-directory]');
const root=resolve(directory);
const types={'.html':'text/html','.js':'text/javascript','.css':'text/css','.svg':'image/svg+xml','.png':'image/png','.json':'application/json'};
const server=createServer(async(req,res)=>{
  try {
    let pathname=decodeURIComponent(new URL(req.url,'http://localhost').pathname);
    if(!pathname.startsWith('/latch/')){res.writeHead(404);res.end();return;}
    if(pathname.endsWith('/'))pathname+='index.html';
    const path=resolve(root,pathname.slice('/latch/'.length));
    if(!path.startsWith(root+'/')){res.writeHead(403);res.end();return;}
    const content=await readFile(path);
    res.writeHead(200,{'Content-Type':types[extname(path)]||'text/plain'});res.end(content);
  }catch{res.writeHead(404);res.end();}
});
let browser;
try {
  server.listen(0,'127.0.0.1');await once(server,'listening');
  const base=`http://127.0.0.1:${server.address().port}/latch/`;
  browser=await openBrowser(chromium);
  const {cdp,evaluate,waitFor,screenshot}=browser;
  async function viewport(width,height=900){await cdp('Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:false});}
  async function navigate(path){
    await evaluate(`globalThis.__siteNavigation='pending'`);
    await cdp('Page.navigate',{url:base+path});
    await waitFor(`globalThis.__siteNavigation===undefined && document.readyState==='complete' && !!document.querySelector('#main')`);
  }
  const click=selector=>evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
  await mkdir('target/site-review',{recursive:true});
  await viewport(1440);await navigate('');
  await waitFor(`!document.querySelector('[data-search]').hidden && document.querySelector('#web-panel').hidden`);
  // Below-fold screenshots are lazy-loaded in production; request them explicitly
  // before asserting asset availability instead of depending on preload timing.
  await evaluate(`[...document.images].forEach(image=>{image.loading='eager';})`);
  await waitFor(`[...document.images].every(image=>image.complete && image.naturalWidth>0)`);
  await screenshot('target/site-review/home-desktop.png');
  await click('#tab-web');
  assert.equal(await evaluate(`document.querySelector('#web-panel').hidden`),false);
  await cdp('Input.dispatchKeyEvent',{type:'keyDown',key:'Home',code:'Home'});
  // Explicitly focus the selected tab before checking roving keyboard behavior.
  await evaluate(`document.querySelector('#tab-web').focus()`);
  await cdp('Input.dispatchKeyEvent',{type:'keyDown',key:'ArrowLeft',code:'ArrowLeft'});
  assert.equal(await evaluate(`document.activeElement.id`),'tab-terminal');
  assert.equal(await evaluate(`document.querySelector('#terminal-panel').hidden`),false);
  console.log('Home, screenshot assets, and keyboard tabs passed.');
  await navigate('docs/introduction/');
  await screenshot('target/site-review/docs-desktop.png');
  assert.equal(await evaluate(`document.querySelector('.sidebar [aria-current="page"]').textContent`),'Introduction');
  await cdp('Input.dispatchKeyEvent',{type:'keyDown',key:'k',code:'KeyK',modifiers:2});
  await waitFor(`document.querySelector('#search-dialog').open`);
  assert.equal(await evaluate('document.activeElement.id'),'search-input');
  await evaluate(`{const input=document.querySelector('#search-input');input.value='MCP';input.dispatchEvent(new Event('input'));}`);
  await waitFor(`document.querySelectorAll('#search-results a').length>0`);
  assert.match(await evaluate(`document.querySelector('#search-results a').href`),/\/latch\/docs\//);
  await screenshot('target/site-review/search.png');
  await cdp('Input.dispatchKeyEvent',{type:'keyDown',key:'ArrowDown',code:'ArrowDown'});
  assert.equal(await evaluate('document.activeElement.tagName'),'A');
  await cdp('Input.dispatchKeyEvent',{type:'keyDown',key:'Escape',code:'Escape',windowsVirtualKeyCode:27});
  await cdp('Input.dispatchKeyEvent',{type:'keyUp',key:'Escape',code:'Escape',windowsVirtualKeyCode:27});
  await waitFor(`!document.querySelector('#search-dialog').open`);
  await click('[data-search]');
  await evaluate(`{const input=document.querySelector('#search-input');input.value='no-such-topic-912391';input.dispatchEvent(new Event('input'));}`);
  await waitFor(`document.querySelector('#search-status').textContent.startsWith('No results')`);
  await click('#search-close');
  await cdp('Browser.grantPermissions',{origin:new URL(base).origin,permissions:['clipboardReadWrite','clipboardSanitizedWrite']});
  // Verify the copied action with localhost clipboard permissions.
  await click('.copy-button');
  await waitFor(`document.querySelector('.copy-button').textContent==='Copied'`);
  console.log('Search, Escape, keyboard results, and clipboard passed.');
  for(const path of ['', 'docs/installation/', 'docs/architecture/', 'docs/config-reference/', 'docs/benchmark-results/']) {
    for(const width of [1440,1024,768,390,320]) {
      await viewport(width);await navigate(path);
      assert.equal(await evaluate('document.documentElement.scrollWidth<=innerWidth'),true,`page overflow at ${width}: ${path}`);
      if(width<=760 && path) {
        assert.equal(await evaluate(`document.querySelector('.sidebar').open`),false);
        await click('.sidebar summary');
        assert.equal(await evaluate(`document.querySelector('.sidebar').open`),true);
        await click('.sidebar summary');
      }
    }
  }
  await viewport(390,844);await navigate('');await screenshot('target/site-review/home-mobile.png');
  await navigate('docs/skills-mcp/');await screenshot('target/site-review/docs-mobile.png');
  await cdp('Emulation.setScriptExecutionDisabled',{value:true});
  await navigate('docs/installation/');
  assert.equal(await evaluate(`!!document.querySelector('.sidebar a[href="../skills-mcp/"]')`),true);
  assert.equal(await evaluate(`document.querySelectorAll('pre code').length>0`),true);
  await navigate('');
  assert.equal(await evaluate(`document.querySelectorAll('figure:not([hidden])').length`),2);
  console.log('Browser checks passed: project subpath, search/keyboard, clipboard, tabs, 5 viewport widths, mobile menu, and no-JS navigation.');
} finally {
  await browser?.close();server.closeAllConnections();server.close();
}
