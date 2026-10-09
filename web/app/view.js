import {state,session,metadata,profile,escapeHtml as e,title,cellParts,cellText,mediaUrl} from './state.js';
import {icon} from './icons.js';

const $ = selector => document.querySelector(selector);
let renderedCells = '';
let renderedSession = null;
export function markdown(text) {
  const inline = value => e(value).replace(/`([^`]+)`/g,'<code>$1</code>').replace(/\*\*([^*]+)\*\*/g,'<strong>$1</strong>');
  const lines = String(text || '').split('\n');
  let code = false; let list = false; let html = '';
  for (const line of lines) {
    if (line.startsWith('```')) { if (list) {html += '</ul>';list=false;} html += code ? '</code></pre>' : '<pre class="code-block"><code>'; code = !code; continue; }
    if (code) {html += e(line)+'\n';continue;}
    if (/^[-*] /.test(line)) {if (!list) {html += '<ul>';list=true;} html += `<li>${inline(line.slice(2))}</li>`;continue;}
    if (list) {html += '</ul>';list=false;}
    if (/^#{1,4} /.test(line)) html += `<h3>${inline(line.replace(/^#+ /,''))}</h3>`;
    else if (line.trim()) html += `<p>${inline(line)}</p>`;
  }
  if (code) html += '</code></pre>';
  if (list) html += '</ul>';
  return html;
}
function tools(label,raw,key,status='') {
  return `<details class="tool-card" data-detail-key="${e(key)}"><summary>${icon(status === 'Failed' ? 'bug' : 'terminal')}<span>${e(label)}</span><span class="tool-meta">${e(status)}</span>${icon('chevron')}</summary><pre class="tool-output">${e(raw)}</pre></details>`;
}
function cellView(cell,index) {
  const [type,data] = cellParts(cell);
  if (type === 'User') return `<article class="message user"><div class="user-bubble">${e(data.text)}${(data.media||[]).map(m=>`<a href="${mediaUrl(m)}" target="_blank" rel="noopener"><img class="message-image" src="${mediaUrl(m)}" alt="${e(m.display_name || 'Attached image')}"></a>`).join('')}</div></article>`;
  let body;
  if (state.raw) body = `<pre class="tool-output">${e(cellText(cell))}</pre>`;
  else if (type === 'Assistant') body = markdown(data.text);
  else if (type === 'Exploration') body = data.operations.map(o=>tools(o.label,o.raw || o.diagnostic,o.call_id,o.status)).join('');
  else if (type === 'Command' || type === 'Validation') body = tools(type === 'Validation' ? `Verify: ${data.requirement}` : data.command,data.raw || data.output,data.call_id,data.status);
  else if (type === 'Patch') body = data.files.map((f,i)=>`<div class="result-strip">${icon('file')}<span>${e(f.path)}</span><span class="diff-plus">+${e(f.additions)}</span><span class="diff-minus">−${e(f.deletions)}</span><button class="change-button" data-cell-diff="${index}" data-file="${i}">View changes ${icon('arrow-right')}</button></div>${f.diagnostic ? `<p class="form-error">${e(f.diagnostic)}</p>` : ''}`).join('');
  else if (type === 'Diff') body = `<div class="result-strip">${icon('file')}<span>Workspace diff · ${data.document.files.length} files</span><button class="change-button" data-cell-diff="${index}">View changes ${icon('arrow-right')}</button></div>`;
  else if (type === 'AgentTask') body = tools(`${title(data.operation)} · ${data.task_name || 'Agents'}`,data.raw || data.summary,data.call_id,data.status);
  else if (type === 'AgentReport') body = `<h3>${e(data.task_name)} · ${e(title(data.status))}</h3>${markdown(data.summary)}`;
  else body = `<p class="${type === 'Error' ? 'form-error' : 'notice-text'}">${e(data.text)}</p>`;
  return `<article class="message assistant"><div class="assistant-label"><img src="./mark.svg" alt="">Latch</div><div class="assistant-body">${body}</div><div class="message-actions"><button class="icon-button" data-copy="${index}" aria-label="Copy response" title="Copy response">${icon('copy')}</button><button class="icon-button" data-raw="${index}" aria-label="Inspect raw output" title="Inspect raw output">${icon('file')}</button></div></article>`;
}
export function renderConversation() {
  const current = session();
  const cells = current.cells || [];
  const show = cells.length > 0 || !!current.busy;
  $('#welcome').hidden = show;
  $('#conversation').hidden = !show;
  $('#conversation-title').textContent = state.sessions.find(s=>s.id===current.session_id)?.prompt_preview || 'New conversation';
  const scroll = $('#view-scroll');
  const stick = scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 90;
  const key = JSON.stringify([current.session_id,cells,state.raw]);
  if (key !== renderedCells) {
    const open = new Set([...document.querySelectorAll('details[open][data-detail-key]')].map(d=>d.dataset.detailKey));
    $('#messages').innerHTML = cells.map(cellView).join('') + '<article class="message assistant" id="streaming-message" hidden><div class="assistant-label"><img src="./mark.svg" alt="">Latch</div><div class="assistant-body" id="streaming-text"></div></article>';
    document.querySelectorAll('[data-detail-key]').forEach(d=>{d.open=open.has(d.dataset.detailKey);});
    renderedCells = key;
  }
  $('#streaming-message').hidden = !current.streaming;
  if (current.streaming) $('#streaming-text').innerHTML = markdown(current.streaming);
  $('#running-indicator').hidden = !current.busy || (current.pending_permissions || []).length > 0;
  $('#running-label').textContent = current.streaming ? 'Writing a response' : 'Working in your workspace';
  if (stick || renderedSession !== current.session_id) requestAnimationFrame(()=>{scroll.scrollTop=scroll.scrollHeight;});
  renderedSession = current.session_id;
}
export function renderSessions() {
  const search = $('#session-search').value.toLowerCase();
  const list = state.sessions.filter(s=>(s.prompt_preview || 'New conversation').toLowerCase().includes(search));
  $('#sessions').innerHTML = list.length ? list.map(s=>`<button class="session-item ${session().session_id===s.id?'active':''}" data-session="${e(s.id)}" ${session().session_id===s.id?'aria-current="page"':''}>${icon('chat')}<span class="session-text">${e(s.prompt_preview || 'New conversation')}</span><span class="session-time">${e(new Date(s.updated_at).toLocaleTimeString('en',{hour:'2-digit',minute:'2-digit'}))}</span></button>`).join('') : '<p class="session-empty">No conversations yet.</p>';
}
export function renderHeader() {
  const p = profile();
  $('#model-label').textContent = metadata().setup_required ? 'Set up a provider' : p.model || 'Choose a model';
  $('#mode-label').textContent = title(p.mode.toLowerCase());
  $('#mode-button [data-icon]').innerHTML = icon(p.mode==='ASK'?'chat':p.mode==='PLAN'?'list':'hammer');
  $('#effort-button').textContent = p.effort === 'provider_default' || !p.effort ? 'Default effort' : `${title(p.effort)} effort`;
  const path = state.snapshot?.workspace || '';
  $('#workspace-path').textContent = path;
  $('#workspace-path').title = `${path} · fixed startup directory`;
  $('#workspace-name').textContent = path.replaceAll('\\','/').split('/').filter(Boolean).at(-1) || '/';
  $('.version').textContent = `v${state.snapshot?.version || '0.2.3'}`;
}
export function renderComposer() {
  const current = session();
  const prompt = $('#prompt');
  prompt.style.height = 'auto';prompt.style.height = Math.min(prompt.scrollHeight,180)+'px';
  const stopping = current.busy && !prompt.value.trim();
  $('#send-button').innerHTML = icon(stopping?'stop':'arrow-up');
  $('#send-button').disabled = !state.connected || current.starting || !!metadata().setup_required || (!current.busy && !prompt.value.trim());
  $('#send-button').setAttribute('aria-label',stopping?'Stop response':current.busy?'Add instructions':'Send message');
  prompt.placeholder = current.busy ? 'Add instructions while Latch works…' : 'Ask anything about your workspace…';
  $('#composer-status').textContent = metadata().setup_required ? 'Configure a provider to start' : !state.connected ? 'Reconnecting…' : current.starting ? 'Opening your conversation…' : (current.pending_permissions||[]).length ? 'Waiting for your approval' : current.busy ? 'Working · add instructions at any time' : 'Ready when you are';
  for (const id of ['new-chat','mode-button','effort-button','model-button']) $(`#${id}`).disabled = current.busy || current.starting || (['effort-button','model-button'].includes(id) && !!metadata().setup_required && metadata().setup_paths?.config_exists===false);
}
export function renderPermission() {
  const request = session().pending_permissions?.[0];
  $('#permission-card').hidden = !request;
  if (request) $('#permission-request').textContent = JSON.stringify(request,null,2);
}
export function renderFiles() {
  $('#attachments').innerHTML = state.pendingFiles.map((file,index)=>`<div class="attachment"><img src="${file.url || mediaUrl(file.reference)}" alt="Image preview"><span>${e(file.name)}</span><button type="button" data-remove-file="${index}" aria-label="Remove ${e(file.name)}">${icon('close')}</button></div>`).join('');
}
function row(label,value) {return `<div class="detail-row"><span>${e(label)}</span><strong>${e(value ?? 'Unavailable')}</strong></div>`;}
function rawDetails(label,data) {return `<details class="tool-card"><summary>${icon('file')}${e(label)}</summary><pre class="tool-output">${e(JSON.stringify(data,null,2))}</pre></details>`;}
export function renderDetails() {
  const current = session(); const sidebar = current.sidebar || {}; const task = sidebar.task;
  let html = '';
  if (state.detailTab === 'changes') {
    html = `<section class="detail-section"><h3>RECORDED CHANGES</h3>${(sidebar.changes?.entries || []).map(f=>`<div class="file-change">${icon('file')}<code>${e(f.path)}</code><span class="diff-plus">+${f.additions}</span><span class="diff-minus">−${f.deletions}</span></div>`).join('') || '<p>No recorded file changes.</p>'}</section><div class="detail-actions"><button class="secondary-button" data-command="/diff">Workspace diff</button><button class="secondary-button" data-command="/checkpoint">Checkpoint</button><button class="secondary-button" data-command="/undo">Undo latest</button></div>${rawDetails('Ownership and external changes',sidebar.changes || {})}`;
  } else if (state.detailTab === 'agents') {
    html = `<section class="detail-section"><h3>CHILD AGENTS</h3>${(sidebar.subagents?.agents || []).map(a=>`<div class="agent-card"><span class="status-pill">${e(title(a.status))}</span><strong>${e(a.task_name)}</strong><p>${e(a.summary || 'No report yet.')}</p></div>`).join('') || '<p>No child agents in this session.</p>'}</section><section class="detail-section"><h3>AGENT GROUP</h3>${sidebar.group?.identity ? `<p>${e(sidebar.group.identity.name)}</p>${Object.values(sidebar.group.tasks || {}).map(t=>`<div class="agent-card"><strong>${e(t.title || t.description || 'Task')}</strong><p>${e(title(t.status))}</p></div>`).join('')}${rawDetails('Coordination details',sidebar.group)}` : '<p>No active group.</p>'}</section><button class="secondary-button" data-command="/group">Group overview</button>`;
  } else {
    const usage = sidebar.usage || {};
    const cost = current.estimated_cost;
    html = `<section class="detail-section"><h3>CURRENT TASK</h3><p>${e(task?.goal || sidebar.current_request || 'Start a conversation to begin.')}</p>${task ? row('Completion',title(task.completion)) : ''}</section><section class="detail-section"><h3>VERIFICATION</h3>${row('Result',title(current.validation_status || 'pending'))}${(task?.required_validations || []).map(name=>row(name,title(sidebar.evidence?.[name.trim().toLowerCase()] || 'Pending'))).join('')}</section><section class="detail-section"><h3>SESSION</h3>${row('Mode',title(profile().mode.toLowerCase()))}${row('Model',profile().model)}${row('Input tokens',usage.input==null?null:`${usage.input_partial?'≥ ':''}${usage.input.toLocaleString('en')}`)}${row('Output tokens',usage.output==null?null:`${usage.output_partial?'≥ ':''}${usage.output.toLocaleString('en')}`)}${row('Cache read',usage.cache_read==null?null:`${usage.cache_read_partial?'≥ ':''}${usage.cache_read.toLocaleString('en')}`)}${row('Estimated cost',cost?`${cost.partial?'≥ ':''}${cost.amount.toFixed(4)} ${cost.currency}`:null)}</section><div class="detail-actions"><button class="secondary-button" data-command="/context">Inspect context</button><button class="secondary-button" data-command="/compact">Reset working context</button></div>${rawDetails('Context and request usage', {context:sidebar.context,usage:sidebar.usage,run:sidebar.run})}${task?rawDetails('Task and evidence',{task,evidence:sidebar.evidence,validation_stale:sidebar.validation_stale}):''}`;
  }
  $('#detail-content').innerHTML = html;
}
export function showDiff(raw,label='Workspace diff') {
  $('#diff-title').textContent = label;
  $('#diff-meta').textContent = 'Unified diff';
  $('#diff-code').innerHTML = String(raw || 'No changes.').split('\n').map(line=>`<span class="${line.startsWith('+++') || line.startsWith('---') ? 'diff-context' : line.startsWith('+')?'diff-added':line.startsWith('-')?'diff-removed':'diff-context'}">${e(line)}</span>`).join('');
  if (!$('#diff-dialog').open) $('#diff-dialog').showModal();
}
export function showModels() {
  const catalog = metadata().inference_catalog?.providers || [];
  const current = profile();
  state.selectedProvider ||= current.provider_id || catalog[0]?.id;
  const provider = catalog.find(p=>p.id===state.selectedProvider) || catalog[0];
  $('#model-list').innerHTML = catalog.length ? `<label class="field-label" for="model-provider">Provider</label><select class="form-input" id="model-provider">${catalog.map(p=>`<option value="${e(p.id)}" ${p.id===provider?.id?'selected':''}>${e(p.display_name)}</option>`).join('')}</select>${(provider?.models || []).map(m=>`<button class="model-choice ${m.id===current.model && provider.id===current.provider_id?'active':''}" data-model="${e(m.id)}" data-provider="${e(provider.id)}"><span>${icon('spark')}</span><div><strong>${e(m.display_name || m.id)}</strong><small>${e(m.id)}${m.input_modalities.includes('image')?' · vision':''}</small></div>${m.id===current.model && provider.id===current.provider_id?icon('check'):''}</button>`).join('')}` : '<p class="settings-note">Configure a provider in Settings to choose a model.</p>';
  if (!$('#model-dialog').open) $('#model-dialog').showModal();
}

const activityLabels = {idle:'Ready',preparing:'Preparing request',waiting_model:'Waiting for model',thinking:'Thinking',writing:'Writing response',preparing_tool:'Preparing tool',running_tool:'Running tool',waiting_approval:'Waiting for approval',cancelling:'Stopping',completed:'Turn finished',cancelled:'Stopped',error:'Request failed',interrupted:'Interrupted'};
const activePhases = new Set(['preparing','waiting_model','thinking','writing','preparing_tool','running_tool','waiting_approval','cancelling']);
function elapsedText(seconds) {seconds=Math.max(0,Math.floor(seconds));return seconds<60?`${seconds}s`:`${Math.floor(seconds/60)}m ${seconds%60}s`;}
export function renderActivity() {
  const current=session();const a=current.sidebar?.activity || {};
  const phase=current.starting?'preparing':a.phase || 'idle';
  const active=activePhases.has(phase);const now=(state.snapshot?.server_time || Date.now()) + (state.receivedAt?performance.now()-state.receivedAt:0);
  const since=value=>value?Math.max(0,(now-Date.parse(value))/1000):0;
  const quiet=since(a.last_activity_at);
  $('#activity-label').textContent=state.connected?(activityLabels[phase] || 'Working'):'Connection lost';
  $('#activity-subject').textContent=a.subject || (active?a.model || profile().model || '': '');
  $('#activity-elapsed').textContent=active && a.phase_since?`${elapsedText(since(a.phase_since))} in this phase`:'';
  $('#activity-last').textContent=active && a.last_activity_at?`Activity ${elapsedText(quiet)} ago`:'';
  $('#activity-dot').classList.toggle('active',active && state.connected);
  $('#activity-dot').classList.toggle('attention',phase==='waiting_approval' || !state.connected);
  let hint='';
  if(!state.connected) hint='Reconnecting to Latch. Closing this page does not stop the task.';
  else if(phase==='waiting_approval') hint='Latch is waiting for your decision.';
  else if(phase==='thinking') hint='The provider is sending reasoning activity.';
  else if(phase==='waiting_model') hint=a.signal==='connected'||a.signal==='receiving'?'Provider stream connected. Waiting for response text.':'Request sent. Waiting for provider activity.';
  else if(phase==='running_tool') hint='The tool is running, including sandbox setup and cleanup.';
  else if(phase==='cancelling') hint='Cancelling the request and cleaning up processes.';
  else if(phase==='completed') hint='The turn ended. Task verification is shown in Overview.';
  else if(phase==='error') hint='Review the error in the conversation before retrying.';
  if(state.connected && active && quiet>=30 && !['waiting_approval','cancelling'].includes(phase)) hint=`No new activity for ${elapsedText(quiet)}. Latch is connected; the operation is still pending. You can Stop.`;
  $('#activity-hint').textContent=hint;
}
