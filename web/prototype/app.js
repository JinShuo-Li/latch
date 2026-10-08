/* Review-only UI. Fixtures and timers never invoke Latch, tools, or a provider. */
'use strict';
const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];
const paths = {
  panel: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M9 4v16"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  search: '<circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 4 4"/>',
  folder: '<path d="M3 7a2 2 0 0 1 2-2h5l2 2h7a2 2 0 0 1 2 2v10H3Z"/>',
  lock: '<rect x="6" y="10" width="12" height="10" rx="2"/><path d="M8 10V7a4 4 0 0 1 8 0v3"/>',
  settings: '<path d="m9 3-1 3-3 1v3l-2 2 2 2v3l3 1 1 3h6l1-3 3-1v-3l2-2-2-2V7l-3-1-1-3Z"/><circle cx="12" cy="12" r="3"/>',
  chevron: '<path d="m7 10 5 5 5-5"/>',
  sliders: '<path d="M4 6h6m4 0h6M4 12h12m4 0h0M4 18h2m4 0h10"/><circle cx="12" cy="6" r="2"/><circle cx="18" cy="12" r="2"/><circle cx="8" cy="18" r="2"/>',
  compass: '<circle cx="12" cy="12" r="9"/><path d="m16 8-2.5 5.5L8 16l2.5-5.5Z"/>',
  bug: '<rect x="7" y="7" width="10" height="13" rx="5"/><path d="m9 3 2 4m4-4-2 4M3 10h4m10 0h4M3 15h4m10 0h4M5 21l3-3m8 0 3 3M12 8v12"/>',
  spark: '<path d="m12 3 2.5 6.5L21 12l-6.5 2.5L12 21l-2.5-6.5L3 12l6.5-2.5Z"/>',
  'arrow-right': '<path d="M4 12h15m-6-6 6 6-6 6"/>',
  'arrow-up': '<path d="M12 19V5m-6 6 6-6 6 6"/>',
  hammer: '<path d="m14 4 6 6-3 3-2-2L7 21l-4-4 10-8-2-2Z"/>',
  list: '<path d="M9 6h11M9 12h11M9 18h11M4 6h0M4 12h0M4 18h0"/>',
  chat: '<path d="M21 14a3 3 0 0 1-3 3H8l-5 4V6a3 3 0 0 1 3-3h12a3 3 0 0 1 3 3Z"/>',
  shield: '<path d="m12 3 8 3v6c0 5-8 9-8 9s-8-4-8-9V6Z"/><path d="m8 12 3 3 5-6"/>',
  close: '<path d="m6 6 12 12M6 18 18 6"/>',
  file: '<path d="M14 3H5v18h14V8Z"/><path d="M14 3v5h5M8 13h8M8 17h6"/>',
  check: '<path d="m5 12 4 4L19 6"/>',
  circle: '<circle cx="12" cy="12" r="8"/>',
  copy: '<rect x="8" y="8" width="12" height="13" rx="2"/><path d="M16 8V3H3v13h5"/>',
  terminal: '<path d="m5 7 5 5-5 5M13 17h6"/>',
  stop: '<rect x="7" y="7" width="10" height="10" rx="1"/>',
};
function icon(name) { return `<svg viewBox="0 0 24 24" aria-hidden="true">${paths[name] || paths.circle}</svg>`; }
function hydrateIcons(root = document) { $$('[data-icon]', root).forEach(el => { el.innerHTML = icon(el.dataset.icon); }); }
function escapeHtml(value) { return String(value).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])); }
const fixtureText = `The parser assumed every record contained a value. An empty line reached parse_value and returned the wrong error. I added an explicit empty-input guard and a regression test. The public API stays the same. Example verification: 12 tests passed.`;
const fixtureBody = `<p>I found the issue. An empty record was reaching the value parser before the input was checked.</p><details class="tool-card"><summary>${icon('search')}<span>Explored the parser and its tests</span><span class="tool-meta">3 files</span>${icon('chevron')}</summary><div class="tool-output">read · crates/latch-kernel/src/parser.rs\nread · crates/latch-kernel/tests/parser.rs\nsearch · parse_value / EmptyInput</div></details><p>I added a small guard for empty input and a regression test. The public API stays the same.</p><details class="tool-card"><summary>${icon('terminal')}<span>Checked the change</span><span class="tool-meta">Example · passed</span>${icon('chevron')}</summary><div class="tool-output">$ cargo test -p latch-kernel parser\n\nrunning 12 tests\ntest result: ok. 12 passed; 0 failed\n\nIllustrative output — no command was executed.</div></details><h3>What changed</h3><ul><li>Empty records now return <code>ParseError::EmptyInput</code>.</li><li>A regression test covers whitespace-only input.</li></ul><div class="result-strip">${icon('check')}<span>Example checks passed</span><button class="change-button" data-show-diff>View changes ${icon('arrow-right')}</button></div>`;
let sessions = [
  { id: 'parser', title: 'Fix the parser edge case', time: '10m', example: true, messages: [{role:'user',text:'Fix the failing parser test without changing the public API.'},{role:'assistant',text:fixtureText,html:fixtureBody}] },
  { id: 'architecture', title: 'Understand the architecture', time: '1h', example: true, messages: [{role:'user',text:'Give me a quick overview of how Latch is put together.'},{role:'assistant',text:'Latch has four first-party Rust crates: latch-protocol defines shared event and model schemas; latch-kernel owns execution, providers, tools and durable state; latch-tui presents the terminal interface; latch-cli wires the application together.',html:'<p>Latch has four first-party Rust crates, each with a clear responsibility.</p><ul><li><code>latch-protocol</code> — shared event and model schemas.</li><li><code>latch-kernel</code> — the agent loop, providers, tools, and durable state.</li><li><code>latch-tui</code> — the terminal interface.</li><li><code>latch-cli</code> — configuration and application wiring.</li></ul><p>The Web interface will share the same kernel, session history, and execution boundaries.</p>'}] },
  { id: 'feature', title: 'Plan the next feature', time: '2h', example: true, messages: [{role:'user',text:'How should we approach a computer use plugin?'},{role:'assistant',text:'Start by defining the actions and their permissions. A future operation view could show screenshots, action history, and a way to pause. The plugin should use the same kernel tool and approval paths. This prototype does not implement computer use.',html:'<p>I’d start with a small set of explicit actions and their permissions.</p><ul><li>Show the current screenshot alongside the conversation.</li><li>Keep a visible action history.</li><li>Provide a clear way to pause and take over.</li><li>Route actions through the shared kernel’s tools and approvals.</li></ul><p>A future operation view can sit beside this conversation. Computer use is not implemented in this prototype.</p>'}] },
];
let activeId = null;
let mode = 'Work';
let selectedModel = 'DeepSeek Flash';
let selectedEffort = 'Low';
let activeTab = 'task';
let workspacePath = '/home/lijs/work/latch';
let running = false;
let generation = 0;
let timers = [];
let pendingFiles = [];
let settingsTab = 'general';
let safety = 'Standard';
let permissions = 'Human';
let toastTimer;
function currentSession() { return sessions.find(s => s.id === activeId); }
function toast(message) { $('#toast').textContent = message; $('#toast').hidden = false; clearTimeout(toastTimer); toastTimer = setTimeout(() => { $('#toast').hidden = true; }, 3500); }
function renderSessions() {
  const query = $('#session-search').value.toLowerCase();
  const visible = sessions.filter(s => s.title.toLowerCase().includes(query));
  $('#sessions').innerHTML = visible.length ? visible.map(s => `<button class="session-item ${s.id === activeId ? 'active' : ''}" data-session="${s.id}" ${s.id === activeId ? 'aria-current="page"' : ''}>${icon('chat')}<span class="session-text">${escapeHtml(s.title)}</span><span class="session-time">${s.time}</span></button>`).join('') : '<p class="session-empty">No matching conversations.</p>';
}
function renderMessages() {
  const session = currentSession();
  $('#welcome').hidden = !!session;
  $('#conversation').hidden = !session;
  $('#conversation-title').textContent = session ? `${session.title}${session.example ? ' · Example' : ' · Demo'}` : '';
  $('#messages').innerHTML = session ? session.messages.map((m, index) => m.role === 'user'
    ? `<article class="message user"><div class="user-bubble">${escapeHtml(m.text)}${m.files?.length ? `<div class="message-files">${m.files.map(escapeHtml).join(' · ')}</div>` : ''}</div></article>`
    : `<article class="message assistant"><div class="assistant-label"><img src="./mark.svg" alt="">Latch<span class="demo-label">${session.example ? 'Example' : 'Demo response'}</span></div><div class="assistant-body">${m.html || `<p>${escapeHtml(m.text).replace(/\n/g, '<br>')}</p>`}</div><div class="message-actions"><button class="icon-button" data-copy="${index}" aria-label="Copy response" title="Copy response">${icon('copy')}</button><button class="icon-button" data-raw="${index}" aria-label="Show raw response" title="Show raw response">${icon('file')}</button></div></article>`).join('') : '';
}
function renderDetails() {
  const session = currentSession();
  const sample = session?.id === 'parser';
  if (!session) {
    $('#detail-content').innerHTML = `<div class="empty-details">${icon('list')}<h3>A clear view of the work</h3><p>Task progress, checks, and file changes will appear here as you work.</p></div>`;
    return;
  }
  if (activeTab === 'changes') {
    $('#detail-content').innerHTML = sample ? `<section class="detail-section"><h3>EXAMPLE FILE CHANGES</h3><button class="file-change" data-show-diff>${icon('file')}<code>src/parser.rs</code><span class="diff-plus">+4</span><span class="diff-minus">−1</span></button><div class="file-change">${icon('file')}<code>tests/parser.rs</code><span class="diff-plus">+12</span></div></section><p class="settings-note">These are illustrative changes. The prototype does not edit workspace files.</p>` : '<div class="empty-details"><h3>No file changes</h3><p>This prototype does not modify your workspace.</p></div>';
  } else if (activeTab === 'agents') {
    $('#detail-content').innerHTML = '<section class="detail-section"><h3>EXAMPLE AGENT ACTIVITY</h3><div class="agent-card"><span class="status-pill">Example</span><strong>Test reviewer</strong><p>Review the test coverage and report missing edge cases.</p></div><p>This shows where child-agent reports will appear.</p></section><div class="detail-section"><h3>AGENT GROUPS</h3><p>Shared tasks and coordination will be available here when a group is active.</p></div>';
  } else {
    $('#detail-content').innerHTML = `<section class="detail-section"><h3>${sample ? 'EXAMPLE TASK' : 'CURRENT CONVERSATION'}</h3><p>${escapeHtml(session.messages[0]?.text || session.title)}</p>${sample ? `<div class="task-step">${icon('check')}Inspect the failing test</div><div class="task-step">${icon('check')}Patch the empty-input case</div><div class="task-step">${icon('check')}Verify the public API is preserved</div>` : `<div class="task-step pending">${icon('circle')}${running ? 'Preview response in progress' : 'UI preview only'}</div>`}</section><section class="detail-section"><h3>VERIFICATION</h3><div class="detail-row"><span>Result</span><span class="status-pill">${sample ? 'Example · passed' : 'Not executed'}</span></div><div class="detail-row"><span>Tests</span><strong>${sample ? '12 / 12 (example)' : '—'}</strong></div><div class="detail-row"><span>Workspace changes</span><strong>${sample ? '2 files (example)' : 'None'}</strong></div></section><section class="detail-section"><h3>SESSION</h3><div class="detail-row"><span>Mode</span><strong>${mode}</strong></div><div class="detail-row"><span>Model</span><strong>${selectedModel}</strong></div><div class="detail-row"><span>Context used</span><strong>${sample ? '24% (example)' : '—'}</strong></div>${sample ? '<div class="meter"><span></span></div>' : ''}<div class="detail-row"><span>Tokens</span><strong>${sample ? '6,240 (example)' : '—'}</strong></div><div class="detail-row"><span>Cost</span><strong>Unavailable</strong></div></section>`;
  }
}
function setDetails(open) { $('#details-panel').hidden = !open; $('#toggle-details').setAttribute('aria-expanded', String(open)); $('#toggle-details').setAttribute('aria-label', open ? 'Close task details' : 'Open task details'); if (open) renderDetails(); }
function scrollBottom() { requestAnimationFrame(() => { $('#view-scroll').scrollTop = $('#view-scroll').scrollHeight; }); }
function resetRun() {
  generation++;
  timers.forEach(clearTimeout);
  timers = [];
  running = false;
  $('#permission-card').hidden = true;
  $('#running-indicator').hidden = true;
  $('#composer-status').textContent = 'Ready when you are';
  $('#prompt').placeholder = 'Ask anything about your workspace…';
  updateComposer();
}
function switchSession(id) {
  if (running) { toast('Stop the preview response before switching conversations.'); return; }
  activeId = id;
  resetRun();
  clearFiles();
  $('#prompt').value = '';
  updateComposer();
  renderSessions(); renderMessages(); renderDetails();
  $('#app').classList.remove('mobile-sidebar-open');
  scrollBottom();
}
function newChat() {
  if (running) { toast('Stop the preview response before starting a new conversation.'); return; }
  switchSession(null);
  $('#prompt').focus();
}
function updateComposer() {
  const prompt = $('#prompt');
  prompt.style.height = 'auto';
  prompt.style.height = Math.min(prompt.scrollHeight, 180) + 'px';
  $('#send-button').disabled = !running && !prompt.value.trim();
  $('#send-button').innerHTML = icon(running && !prompt.value.trim() ? 'stop' : 'arrow-up');
  const label = running ? (prompt.value.trim() ? 'Add instructions' : 'Stop preview response') : 'Send message';
  $('#send-button').setAttribute('aria-label', label);
  $('#send-button').title = label;
}
function schedule(callback, delay) {
  const ticket = generation;
  timers.push(setTimeout(() => { if (ticket === generation) callback(); }, delay));
}
function finishDemo(approved = null) {
  const session = currentSession();
  if (!session) return;
  $('#permission-card').hidden = true;
  const text = approved === false
    ? 'You denied the example command. Nothing was executed. In the connected application, Latch will receive your decision and continue within the remaining permissions.'
    : approved === true
      ? 'You approved the example request. This preview did not run the command. In the connected application, this decision will go to the shared kernel, and the real output will appear here.'
      : `I’d start by reading the repository instructions and the relevant code, then ${mode === 'Ask' ? 'explain what I find.' : mode === 'Plan' ? 'propose a plan for your review.' : 'make a focused change and verify it.'}\n\nThis is a simulated response so you can review the conversation layout. No model was contacted and no workspace files were changed.`;
  session.messages.push({role:'assistant',text});
  resetRun(); renderMessages(); renderDetails(); scrollBottom();
}
function showPermission() {
  $('#permission-card').hidden = false;
  $('#running-indicator').hidden = true;
  $('#composer-status').textContent = 'Waiting for your approval · demo';
  $('#permission-request').textContent = JSON.stringify({tool:'shell',arguments:{command:'cargo test -p latch-kernel parser',timeout_ms:120000},reason:'Run the focused parser checks',capabilities:['command_execution'],workspace:workspacePath}, null, 2);
}
function sendMessage(event) {
  event?.preventDefault();
  const text = $('#prompt').value.trim();
  if (running && !text) { stopRun(); return; }
  if (!text) return;
  if (text === '/diff') { $('#prompt').value = ''; updateComposer(); $('#diff-dialog').showModal(); return; }
  if (text === '/model' || text === '/setup') { $('#prompt').value = ''; updateComposer(); text === '/model' ? showModels() : openSettings('providers'); return; }
  if (text === '/resume') { $('#prompt').value = ''; updateComposer(); $('#app').classList.remove('sidebar-collapsed'); $('#app').classList.add('mobile-sidebar-open'); $('#session-search').focus(); return; }
  if (text === '/help') { $('#prompt').value = ''; updateComposer(); toast('Preview commands: /diff · /model · /setup · /resume. Other commands are demo messages.'); return; }
  if (!activeId) {
    const session = {id:crypto.randomUUID(),title:text.length > 29 ? text.slice(0,29) + '…' : text,time:'now',example:false,messages:[]};
    sessions.unshift(session); activeId = session.id;
  }
  const wasRunning = running;
  currentSession().messages.push({role:'user',text:wasRunning ? `Additional instructions: ${text}` : text,files:pendingFiles.map(f => f.name)});
  $('#prompt').value = ''; clearFiles();
  renderSessions(); renderMessages(); scrollBottom();
  if (wasRunning) { toast('Additional instructions added to this demo conversation.'); updateComposer(); return; }
  running = true;
  $('#running-indicator').hidden = false;
  $('#running-label').textContent = 'Previewing the next step';
  $('#composer-status').textContent = 'Working · add instructions at any time';
  $('#prompt').placeholder = 'Add instructions while Latch works…';
  updateComposer(); renderDetails();
  schedule(mode === 'Work' ? showPermission : () => finishDemo(), 1800);
}
function stopRun() {
  if (!running) return;
  currentSession()?.messages.push({role:'assistant',text:'Preview response stopped. Nothing was executed. You can continue the conversation whenever you’re ready.'});
  resetRun(); renderMessages(); renderDetails(); scrollBottom();
}
function resolvePermission(approved) {
  if (!running || $('#permission-card').hidden) return;
  $('#permission-card').hidden = true;
  $('#running-indicator').hidden = false;
  $('#running-label').textContent = approved ? 'Previewing the result' : 'Continuing without this command';
  schedule(() => finishDemo(approved), 900);
}
function clearFiles() { pendingFiles.forEach(file => URL.revokeObjectURL(file.url)); pendingFiles = []; renderFiles(); }
function renderFiles() {
  $('#attachments').innerHTML = pendingFiles.map((file,index) => `<div class="attachment"><img src="${file.url}" alt="Image preview"><span>${escapeHtml(file.name)}</span><button type="button" data-remove-file="${index}" aria-label="Remove ${escapeHtml(file.name)}">${icon('close')}</button></div>`).join('');
}
function addFiles(files) {
  [...files].forEach(file => {
    if (!['image/png','image/jpeg','image/webp'].includes(file.type)) { toast('Choose a PNG, JPEG, or WebP image.'); return; }
    if (file.size > 10 * 1024 * 1024) { toast('Choose an image smaller than 10 MB for this preview.'); return; }
    if (pendingFiles.length >= 5) { toast('Up to 5 images can be previewed at once.'); return; }
    pendingFiles.push({name:file.name,url:URL.createObjectURL(file)});
  });
  renderFiles();
}
function setMode(value) {
  if (running) { toast('Stop or finish the preview response before changing mode.'); return; }
  mode = value;
  $('#mode-label').textContent = mode;
  $('#mode-button [data-icon]').dataset.icon = mode === 'Work' ? 'hammer' : mode === 'Ask' ? 'chat' : 'list';
  hydrateIcons($('#mode-button'));
  $('#mode-menu').hidden = true;
  $('#mode-button').setAttribute('aria-expanded', 'false');
  renderDetails();
}
function showModels() {
  $('#model-list').innerHTML = [{name:'DeepSeek Flash',provider:'OpenCode Go',mark:'D',description:'Fast, focused everyday coding'}, {name:'Claude Sonnet',provider:'Anthropic',mark:'C',description:'A thoughtful partner for complex work'}, {name:'GPT',provider:'OpenAI',mark:'G',description:'General coding and reasoning'}].map(m => `<button class="model-choice ${m.name === selectedModel ? 'active' : ''}" data-model="${m.name}"><span>${m.mark}</span><div><strong>${m.name}</strong><small>${m.provider} · ${m.description}</small></div>${m.name === selectedModel ? icon('check') : ''}</button>`).join('');
  $('#model-dialog').showModal();
}
function openSettings(tab = 'general') { settingsTab = tab; renderSettings(); $('#settings-dialog').showModal(); }
function renderSettings() {
  $$('.settings-tabs button').forEach(button => button.classList.toggle('selected', button.dataset.settings === settingsTab));
  if (settingsTab === 'general') {
    $('#settings-content').innerHTML = `<div class="settings-row"><div><label for="theme">Appearance</label><small>A comfortable place to focus.</small></div><select id="theme"><option value="light">Light</option><option value="dark">Dark</option></select></div><div class="settings-row"><div><strong>Language</strong><small>Latch’s default interface language.</small></div><span class="fixed-value">English</span></div><div class="settings-row"><div><strong>Workspace</strong><small>Fixed to the startup directory.</small></div><code class="fixed-value">${escapeHtml(workspacePath)}</code></div><div class="settings-row"><div><strong>Keyboard shortcuts</strong><small>Enter to send · Shift + Enter for a new line<br>Ctrl / ⌘ + K for a new conversation<br>Escape to close panels</small></div></div>`;
    $('#theme').value = document.body.classList.contains('dark') ? 'dark' : 'light';
  } else if (settingsTab === 'providers') {
    $('#settings-content').innerHTML = '<div class="provider-card"><span class="status-pill">Example</span><strong>OpenCode Go</strong><p>Default model: DeepSeek Flash<br>Credentials: environment variable<br>Credential values are never displayed.</p><button class="secondary-button" id="provider-model">Browse example models</button></div><p class="settings-note">The connected version will use the existing provider configuration flow for credentials, discovery, and model settings.</p>';
  } else {
    $('#settings-content').innerHTML = `<div class="settings-row"><div><label for="safety-select">Safety profile</label><small>Controls what operations are permitted.</small></div><select id="safety-select">${['Strict','Standard','Autonomous'].map(v => `<option ${v === safety ? 'selected' : ''}>${v}</option>`).join('')}</select></div><div class="settings-row"><div><label for="permission-select">Permission resolver</label><small>How approval requests are resolved.</small></div><select id="permission-select">${['Auto','Human','AI'].map(v => `<option ${v === permissions ? 'selected' : ''}>${v}</option>`).join('')}</select></div><p class="settings-note">Ask and Plan remain read-only. In this preview, selections only demonstrate the controls; they do not change Latch configuration or permissions.</p>`;
  }
}
hydrateIcons(); renderSessions(); renderMessages(); updateComposer();
$('#new-chat').addEventListener('click', newChat);
$('#home').addEventListener('click', newChat);
$('#sample-session').addEventListener('click', () => switchSession('parser'));
$('#session-search').addEventListener('input', renderSessions);
$('#sessions').addEventListener('click', e => { const button = e.target.closest('[data-session]'); if (button) switchSession(button.dataset.session); });
$('#collapse-sidebar').addEventListener('click', () => { $('#app').classList.add('sidebar-collapsed'); $('#app').classList.remove('mobile-sidebar-open'); });
$('#expand-sidebar').addEventListener('click', () => { $('#app').classList.remove('sidebar-collapsed'); $('#app').classList.toggle('mobile-sidebar-open'); });
$('#toggle-details').addEventListener('click', () => setDetails($('#details-panel').hidden));
$('#close-details').addEventListener('click', () => setDetails(false));
$('#prompt').addEventListener('input', updateComposer);
$('#prompt').addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); sendMessage(); } });
$('#composer').addEventListener('submit', sendMessage);
$$('[data-suggestion]').forEach(button => button.addEventListener('click', () => { $('#prompt').value = button.dataset.suggestion; updateComposer(); $('#prompt').focus(); }));
$('#mode-button').setAttribute('aria-expanded', 'false');
$('#mode-button').addEventListener('click', () => { if (running) { toast('Stop or finish the preview response before changing mode.'); return; } $('#mode-menu').hidden = !$('#mode-menu').hidden; $('#mode-button').setAttribute('aria-expanded', String(!$('#mode-menu').hidden)); if (!$('#mode-menu').hidden) $('[data-mode]').focus(); });
$$('[data-mode]').forEach(button => button.addEventListener('click', () => { setMode(button.dataset.mode); $('#mode-button').focus(); }));
$('#mode-menu').addEventListener('keydown', e => { if (!['ArrowDown','ArrowUp','Home','End'].includes(e.key)) return; e.preventDefault(); const buttons = $$('[data-mode]'); const current = buttons.indexOf(document.activeElement); const next = e.key === 'Home' ? 0 : e.key === 'End' ? buttons.length - 1 : (current + (e.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length; buttons[next].focus(); });
$('#effort-button').addEventListener('click', () => { if (running) { toast('Stop or finish the preview response before changing effort.'); return; } const values = ['Low','Medium','High','Default']; selectedEffort = values[(values.indexOf(selectedEffort) + 1) % values.length]; $('#effort-button').textContent = `${selectedEffort} effort`; });
$('#stop-inline').addEventListener('click', stopRun);
$('#approve').addEventListener('click', () => resolvePermission(true));
$('#deny').addEventListener('click', () => resolvePermission(false));
$('#attach').addEventListener('click', () => $('#file-input').click());
$('#file-input').addEventListener('change', e => { addFiles(e.target.files); e.target.value = ''; });
$('#composer').addEventListener('dragover', e => e.preventDefault());
$('#composer').addEventListener('drop', e => { e.preventDefault(); addFiles(e.dataTransfer.files); });
$('#attachments').addEventListener('click', e => { const button = e.target.closest('[data-remove-file]'); if (!button) return; const [file] = pendingFiles.splice(Number(button.dataset.removeFile),1); URL.revokeObjectURL(file.url); renderFiles(); });
$('#open-settings').addEventListener('click', () => openSettings());
$('#model-button').addEventListener('click', showModels);
$('#model-list').addEventListener('click', e => { const button = e.target.closest('[data-model]'); if (!button) return; if (running) { toast('Stop or finish the preview response before changing model.'); return; } selectedModel = button.dataset.model; $('#model-label').textContent = selectedModel; $('#model-dialog').close(); renderDetails(); });
$$('[data-close]').forEach(button => button.addEventListener('click', () => $(`#${button.dataset.close}`).close()));
$$('dialog').forEach(dialog => dialog.addEventListener('click', e => { if (e.target === dialog) { const rect = dialog.getBoundingClientRect(); if (e.clientX < rect.left || e.clientX > rect.right || e.clientY < rect.top || e.clientY > rect.bottom) dialog.close(); } }));
$$('[data-settings]').forEach(button => button.addEventListener('click', () => { settingsTab = button.dataset.settings; renderSettings(); }));
$('#settings-content').addEventListener('change', e => { if (e.target.id === 'theme') document.body.classList.toggle('dark', e.target.value === 'dark'); if (e.target.id === 'safety-select') safety = e.target.value; if (e.target.id === 'permission-select') permissions = e.target.value; });
$('#settings-content').addEventListener('click', e => { if (e.target.closest('#provider-model')) { $('#settings-dialog').close(); showModels(); } });
$$('[data-tab]').forEach(button => button.addEventListener('click', () => { activeTab = button.dataset.tab; $$('[data-tab]').forEach(b => b.classList.toggle('selected', b === button)); renderDetails(); }));
document.addEventListener('click', async e => {
  if (!e.target.closest('.mode-control')) { $('#mode-menu').hidden = true; $('#mode-button').setAttribute('aria-expanded', 'false'); }
  if (e.target.closest('[data-show-diff]')) $('#diff-dialog').showModal();
  const copy = e.target.closest('[data-copy]');
  if (copy) { try { await navigator.clipboard.writeText(currentSession().messages[Number(copy.dataset.copy)].text); toast('Response copied.'); } catch { toast('Clipboard unavailable. You can select and copy the response.'); } }
  const raw = e.target.closest('[data-raw]');
  if (raw) { const body = raw.closest('.message').querySelector('.assistant-body'); body.innerHTML = `<pre class="tool-output">${escapeHtml(currentSession().messages[Number(raw.dataset.raw)].text)}</pre>`; }
});
document.addEventListener('keydown', e => {
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); if (!$('dialog[open]')) newChat(); }
  if (e.key === 'Escape' && !$('dialog[open]')) { $('#mode-menu').hidden = true; $('#mode-button').setAttribute('aria-expanded','false'); setDetails(false); $('#app').classList.remove('mobile-sidebar-open'); }
});
fetch('./workspace.json').then(response => { if (!response.ok) throw new Error('No workspace metadata'); return response.json(); }).then(data => { if (typeof data.path !== 'string') return; workspacePath = data.path; $('#workspace-path').textContent = data.path; $('#workspace-path').title = data.path + ' · fixed startup directory'; }).catch(() => { /* Static preview retains its explicit example path. */ });
