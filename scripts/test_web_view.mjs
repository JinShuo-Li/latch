// Browser-independent regression for the production header renderer.
import assert from 'node:assert/strict';
import {test} from 'node:test';
import {state} from '../web/app/state.js';
import {renderHeader} from '../web/app/view.js';

test('header uses the server version and never invents a release while loading',()=>{
  const originalDocument=globalThis.document;
  const originalSnapshot=state.snapshot;
  const elements=new Map();
  globalThis.document={querySelector(selector){
    if(!elements.has(selector))elements.set(selector,{});
    return elements.get(selector);
  }};
  try {
    state.snapshot=null;
    renderHeader();
    assert.equal(elements.get('.version').textContent,'');
    state.snapshot={version:'0.3.3',workspace:'/fixture',state:{}};
    renderHeader();
    assert.equal(elements.get('.version').textContent,'v0.3.3');
    state.snapshot={...state.snapshot,version:'0.3.4'};
    renderHeader();
    assert.equal(elements.get('.version').textContent,'v0.3.4');
    delete state.snapshot.version;
    renderHeader();
    assert.equal(elements.get('.version').textContent,'');
  } finally {
    state.snapshot=originalSnapshot;
    if(originalDocument===undefined)delete globalThis.document;
    else globalThis.document=originalDocument;
  }
});
