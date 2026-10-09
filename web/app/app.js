import {api,subscribe} from './transport.js';
import {state,session,metadata,profile,providers,model,cellParts,cellText,title} from './state.js';
import {hydrateIcons,icon} from './icons.js';
import {visibleSessions,renderActivity,renderConversation,renderSessions,renderHeader,renderComposer,renderPermission,renderFiles,renderDetails,showDiff,showModels} from './view.js';
import {renderSettings,settingsPlan,renderSettingsStatus,credentialInput,renderNewModel} from './settings.js';

const $ = selector => document.querySelector(selector);
const $$ = selector => [...document.querySelectorAll(selector)];
let stopSubscription;
let toastTimer;
let sessionRefresh;
let diffRequested = false;
let submitting = false;
let settingsResultAt = null;
let settingsMetadata;
let removedImages = new Set();
let observedSession;
let setupShown;
function toast(message) {$('#toast').textContent=message;$('#toast').hidden=false;clearTimeout(toastTimer);toastTimer=setTimeout(()=>{$('#toast').hidden=true;},4500);}
async function attempt(action) {try{return await action();}catch(error){if(error.status===401)openAuth();else toast(error.message);return null;}}
function openAuth() {stopSubscription?.();state.connected=false;renderComposer();$('#connection-state').textContent='DISCONNECTED';if(!$('#auth-dialog').open)$('#auth-dialog').showModal();}
function setConnection(connected) {state.connected=connected;$('#connection-state').textContent=connected?'CONNECTED':'RECONNECTING';renderComposer();renderActivity();}
async function refreshSessions() {const result=await api('/api/sessions');state.sessions=result.sessions;renderSessions();}
function applySnapshot(snapshot) {
  if (state.snapshot?.instance_id===snapshot.instance_id && state.snapshot.state.sequence > snapshot.state.sequence) return;
  if (state.snapshot && state.snapshot.instance_id!==snapshot.instance_id) {removedImages=new Set();state.pendingFiles=[];}
  state.snapshot=snapshot;state.receivedAt=performance.now();
  if (observedSession!==snapshot.state.session_id) {settingsResultAt=null;state.settingsPending=null;state.settingsMessage='';state.settingsError=false;settingsMetadata=undefined;state.pendingFiles.forEach(f=>{if(f.url)URL.revokeObjectURL(f.url);});state.pendingFiles=[];removedImages=new Set();observedSession=snapshot.state.session_id;state.historyPosition=null;}
  for (const reference of snapshot.state.pending_attachments || []) if (!removedImages.has(reference.id) && !state.pendingFiles.some(f=>f.reference?.id===reference.id)) state.pendingFiles.push({reference,name:reference.display_name || 'Attached image'});
  renderActivity();renderHeader();renderConversation();renderComposer();renderPermission();renderFiles();
  const setupKey=`${snapshot.instance_id}:${snapshot.state.session_id}`;
  if(metadata().setup_required && !session().starting && setupShown!==setupKey) {setupShown=setupKey;openSettings('providers');if(metadata().setup_paths?.config_exists===false){state.settingsProvider='__new';renderSettings();}}
  if (!$('#details-panel').hidden) renderDetails();
  if (diffRequested && metadata().diff!==undefined && !session().busy && !session().starting) {diffRequested=false;showDiff(metadata().diff);}
  if (settingsResultAt!==null) {
    const latest=(session().cells || []).slice(settingsResultAt).map(cellParts).filter(([type])=>type==='Notice'||type==='Error').at(-1);
    if (latest) {
      const pending=state.settingsPending;
      state.settingsError=latest[0]==='Error';
      state.settingsMessage=latest[1].text;
      settingsResultAt=null;state.settingsPending=null;
      if(!state.settingsError) {
        if(pending?.provider) {state.settingsProvider=pending.provider;state.settingsModel=null;}
        if(pending?.remove) {state.settingsProvider=null;state.settingsModel=null;}
        if(pending?.model) {state.settingsModel=pending.model;state.modelField='Transport';}
        if($('#settings-dialog').open)renderSettings();
      }
      toast(state.settingsMessage);
    }
  }
  const nextSettingsMetadata=JSON.stringify([metadata().setup_providers,metadata().setup_models,metadata().setup_required]);
  if(nextSettingsMetadata!==settingsMetadata) {
    settingsMetadata=nextSettingsMetadata;
    if(!state.settingsPending && state.settingsTab==='providers' && $('#settings-dialog').open)renderSettings();
  }
  if($('#settings-dialog').open)renderSettingsStatus();
  if (!sessionRefresh) sessionRefresh=setTimeout(()=>{sessionRefresh=undefined;attempt(refreshSessions);},500);
}
async function connect() {
  const snapshot=await api('/api/bootstrap');applySnapshot(snapshot);await refreshSessions();
  stopSubscription?.();stopSubscription=subscribe(applySnapshot,setConnection,error=>{if(error.status===401)openAuth();else setConnection(false);});
  if($('#auth-dialog').open)$('#auth-dialog').close();
  setConnection(true);
}
async function ensureSession() {
  if(session().session_id)return session().session_id;
  const created=await api('/api/sessions',{method:'POST'});
  applySnapshot(await api('/api/bootstrap'));
  await refreshSessions();return created.session_id;
}
async function sendInput(type,data) {
  const id=await ensureSession();
  const result=await api(`/api/sessions/${id}/commands`,{method:'POST',body:{command_id:crypto.randomUUID(),instance_id:state.snapshot.instance_id,issued_at:state.snapshot.server_time+Math.floor(performance.now()-state.receivedAt),input:{type,...(data===undefined?{}:{data})}}});
  // Acceptance is not durable success; actual results arrive through events.
  applySnapshot(await api('/api/bootstrap'));
  return result;
}
async function newChat() {if(session().busy || session().starting)return;await api('/api/sessions',{method:'POST'});$('#prompt').value='';applySnapshot(await api('/api/bootstrap'));await refreshSessions();$('#app').classList.remove('mobile-sidebar-open');$('#prompt').focus();}
function openSettings(tab='general') {state.settingsTab=tab;state.settingsProvider=null;state.settingsModel=null;renderSettings();if(!$('#settings-dialog').open)$('#settings-dialog').showModal();}
function setDetails(open) {$('#details-panel').hidden=!open;$('#toggle-details').setAttribute('aria-expanded',String(open));$('#toggle-details').setAttribute('aria-label',open?'Close task details':'Open task details');if(open)renderDetails();}
function showCommands() {$('#command-list').innerHTML='';for(const command of state.snapshot?.commands || []){const button=document.createElement('button');button.className='command-choice';button.dataset.paletteCommand=command.name;const code=document.createElement('code');code.textContent=command.name;const text=document.createElement('span');text.textContent=command.description;button.append(code,text);$('#command-list').append(button);}$('#commands-dialog').showModal();}
async function slash(text) {
  const [name,...args]=text.trim().split(/\s+/);
  if (name==='/help') {showCommands();return;}
  if (name==='/setup') {openSettings('providers');return;}
  if (name==='/model') {showModels();addEffortSelector();return;}
  if (name==='/resume') {$('#app').classList.remove('sidebar-collapsed');$('#app').classList.add('mobile-sidebar-open');$('#session-search').focus();return;}
  if (name==='/sidebar') {setDetails($('#details-panel').hidden);return;}
  if (name==='/raw') {state.raw=!state.raw;renderConversation();return;}
  if (name==='/attach') {if(args.length)throw new Error('Use the attachment button to select a file from your browser.');$('#file-input').click();return;}
  if (name==='/attachments') {toast(state.pendingFiles.length?state.pendingFiles.map(f=>f.name).join(', '):'No pending attachments.');return;}
  if (name==='/detach') {if(!args.length || args[0]==='all'){for(let i=state.pendingFiles.length-1;i>=0;i--)await removeFile(i);}else {const index=Number(args[0])-1;if(!Number.isInteger(index)||index<0||index>=state.pendingFiles.length)throw new Error('Use /detach <1-based index> or /detach all.');await removeFile(index);}return;}
  if (name==='/quit'||name==='/exit') {await sendInput('quit');stopSubscription?.();state.connected=false;$('#connection-state').textContent='STOPPED';renderComposer();toast('Web server stopped.');return;}
  if (name==='/diff') {delete metadata().diff;diffRequested=true;}
  const result=await sendInput('submit',{text,media:[]});

  return result;
}
async function submit() {
  if(submitting)return; submitting=true;
  try {
  const text=$('#prompt').value.trim();
  if(session().busy && !text){await sendInput('cancel');return;}
  if(!text)return;
  if(!text.includes('\n') && text.startsWith('/')) {await slash(text);$('#prompt').value='';renderComposer();return;}
  if(metadata().setup_required){openSettings('providers');return;}
  const media=state.pendingFiles.map(f=>f.reference).filter(Boolean);
  await sendInput('submit',{text,media});
  state.pendingFiles.forEach(f=>{removedImages.add(f.reference.id);if(f.url)URL.revokeObjectURL(f.url);});state.pendingFiles=[];
  $('#prompt').value='';state.historyPosition=null;renderFiles();renderComposer();
  const scroll=$('#view-scroll');requestAnimationFrame(()=>{scroll.scrollTop=scroll.scrollHeight;});
  } finally {submitting=false;renderComposer();}
}
async function addFiles(files) {
  const id=await ensureSession();
  for(const file of files) {
    if(!['image/png','image/jpeg','image/webp'].includes(file.type))throw new Error('Choose a PNG, JPEG, or WebP image.');
    if(file.size>5*1024*1024)throw new Error('Choose an image no larger than 5 MiB.');
    const reference=await api(`/api/sessions/${id}/attachments`,{method:'POST',headers:{'Content-Type':file.type,'X-Image-Name':file.name.replace(/[^\x20-\x7e]/g,'_')},body:file});
    removedImages.delete(reference.id);
    if(!state.pendingFiles.some(f=>f.reference?.id===reference.id))state.pendingFiles.push({name:file.name,reference,url:URL.createObjectURL(file)});
    renderFiles();
  }
}
async function removeFile(index) {const file=state.pendingFiles[index];if(!file)return;await api(`/api/sessions/${session().session_id}/attachments/${file.reference.id}`,{method:'DELETE'});removedImages.add(file.reference.id);if(file.url)URL.revokeObjectURL(file.url);state.pendingFiles.splice(index,1);renderFiles();}
function addEffortSelector() {
  const effort = model()?.efforts || [];
  const label=document.createElement('label');label.className='field-label';label.textContent='Reasoning effort';
  const select=document.createElement('select');select.id='live-effort';select.className='form-input';
  for(const value of ['provider_default',...effort]){const option=document.createElement('option');option.value=value;option.textContent=title(value);option.selected=value===profile().effort;select.append(option);}
  label.append(select);$('#model-list').append(label);
}
async function setup(plan,after={}) {
  if(state.settingsPending || session().busy || session().starting)return;
  state.settingsPending=after;state.settingsError=false;state.settingsMessage='Saving configuration…';
  settingsResultAt=(session().cells || []).length;renderSettingsStatus();
  try {await sendInput('setup_apply',plan);}
  catch(error) {settingsResultAt=null;state.settingsPending=null;state.settingsError=true;state.settingsMessage=error.message;renderSettingsStatus();throw error;}
}

hydrateIcons();
try{document.body.classList.toggle('dark',localStorage.getItem('latch-theme')!=='light');}catch{/* Appearance storage is optional. */}
$('#auth-dialog').addEventListener('cancel',e=>e.preventDefault());
$('#auth-form').addEventListener('submit',event=>{event.preventDefault();attempt(async()=>{try{await api('/api/auth',{method:'POST',body:{token:$('#access-token').value.trim()}});$('#access-token').value='';$('#auth-error').textContent='';await connect();}catch(error){$('#auth-error').textContent=error.message;}});});
$('#new-chat').addEventListener('click',()=>attempt(newChat));
$('#home').addEventListener('click',()=>attempt(newChat));
$('#session-search').addEventListener('input',renderSessions);
let pendingSessionDeletion = [];
$('#manage-sessions').addEventListener('click',()=>{state.managingSessions=!state.managingSessions;state.selectedSessions.clear();renderSessions();});
$('#sessions').addEventListener('change',event=>{
  const input=event.target.closest('[data-select-session]');
  if (!input) return;
  if(input.checked)state.selectedSessions.add(input.dataset.selectSession);else state.selectedSessions.delete(input.dataset.selectSession);
  renderSessions();
});
$('#select-visible-sessions').addEventListener('change',event=>{
  for(const s of visibleSessions())if(s.id!==session().session_id){if(event.target.checked)state.selectedSessions.add(s.id);else state.selectedSessions.delete(s.id);}
  renderSessions();
});
$('#delete-sessions').addEventListener('click',()=>{
  pendingSessionDeletion=[...state.selectedSessions];
  if(!pendingSessionDeletion.length)return;
  $('#delete-sessions-error').textContent=pendingSessionDeletion.length>100?'Select at most 100 conversations per deletion.':'';
  $('#confirm-delete-sessions').disabled=pendingSessionDeletion.length>100;
  $('#delete-session-names').replaceChildren(...pendingSessionDeletion.map(id=>{const li=document.createElement('li');li.textContent=`${state.sessions.find(s=>s.id===id)?.prompt_preview || 'New conversation'} · ${id.slice(0,8)}`;return li;}));
  $('#delete-sessions-dialog').showModal();
});
$('#confirm-delete-sessions').addEventListener('click',async()=>{
  const button=$('#confirm-delete-sessions');button.disabled=true;
  try {
    await api('/api/sessions/delete',{method:'POST',body:{ids:pendingSessionDeletion,instance_id:state.snapshot.instance_id}});
    state.selectedSessions.clear();$('#delete-sessions-dialog').close();
    await refreshSessions();toast('Conversations removed from history.');
  } catch(error) {$('#delete-sessions-error').textContent=error.message;}
  finally {button.disabled=false;}
});
$('#sessions').addEventListener('click',event=>{const button=event.target.closest('[data-session]');if(button)attempt(async()=>{await api(`/api/sessions/${button.dataset.session}/activate`,{method:'POST'});$('#prompt').value='';applySnapshot(await api('/api/bootstrap'));await refreshSessions();$('#app').classList.remove('mobile-sidebar-open');});});
$('#collapse-sidebar').addEventListener('click',()=>{$('#app').classList.add('sidebar-collapsed');$('#app').classList.remove('mobile-sidebar-open');});
$('#expand-sidebar').addEventListener('click',()=>{$('#app').classList.remove('sidebar-collapsed');$('#app').classList.toggle('mobile-sidebar-open');});
$('#toggle-details').addEventListener('click',()=>setDetails($('#details-panel').hidden));
$('#close-details').addEventListener('click',()=>setDetails(false));
$$('[data-tab]').forEach(button=>button.addEventListener('click',()=>{state.detailTab=button.dataset.tab;$$('[data-tab]').forEach(b=>b.classList.toggle('selected',b===button));renderDetails();}));
$('#prompt').addEventListener('input',()=>{state.historyPosition=null;renderComposer();});
$('#prompt').addEventListener('keydown',event=>{
  if(event.key==='Enter'&&!event.shiftKey&&!event.isComposing){event.preventDefault();attempt(submit);}
  const history=session().history || [];const prompt=$('#prompt');
  if(event.key==='ArrowUp' && !event.shiftKey && prompt.selectionStart===0 && history.length){event.preventDefault();if(state.historyPosition===null){state.draftBeforeHistory=prompt.value;state.historyPosition=history.length;}state.historyPosition=Math.max(0,state.historyPosition-1);prompt.value=history[state.historyPosition];prompt.setSelectionRange(0,0);renderComposer();}
  if(event.key==='ArrowDown' && !event.shiftKey && state.historyPosition!==null && prompt.selectionEnd===prompt.value.length){event.preventDefault();state.historyPosition=Math.min(history.length,state.historyPosition+1);prompt.value=state.historyPosition===history.length?state.draftBeforeHistory:history[state.historyPosition];renderComposer();}
});
$('#composer').addEventListener('submit',event=>{event.preventDefault();attempt(submit);});
$$('[data-suggestion]').forEach(button=>button.addEventListener('click',()=>{$('#prompt').value=button.dataset.suggestion;renderComposer();$('#prompt').focus();}));
$('#mode-button').addEventListener('click',()=>{$('#mode-menu').hidden=!$('#mode-menu').hidden;$('#mode-button').setAttribute('aria-expanded',String(!$('#mode-menu').hidden));if(!$('#mode-menu').hidden)$('[data-mode]').focus();});
$$('[data-mode]').forEach(button=>button.addEventListener('click',()=>attempt(async()=>{await sendInput('submit',{text:`/mode ${button.dataset.mode.toLowerCase()}`,media:[]});$('#mode-menu').hidden=true;$('#mode-button').focus();})));
$('#mode-menu').addEventListener('keydown',event=>{if(!['ArrowDown','ArrowUp','Home','End'].includes(event.key))return;event.preventDefault();const buttons=$$('[data-mode]');const current=buttons.indexOf(document.activeElement);const next=event.key==='Home'?0:event.key==='End'?buttons.length-1:(current+(event.key==='ArrowDown'?1:-1)+buttons.length)%buttons.length;buttons[next].focus();});
$('#effort-button').addEventListener('click',()=>{showModels();addEffortSelector();$('#live-effort').focus();});
$('#model-button').addEventListener('click',()=>{showModels();addEffortSelector();});
$('#model-list').addEventListener('change',event=>attempt(async()=>{if(event.target.id==='model-provider'){state.selectedProvider=event.target.value;showModels();addEffortSelector();}if(event.target.id==='live-effort'){const p=profile();await sendInput('set_inference_profile',{provider:p.provider_id,model:p.model,effort:event.target.value});$('#model-dialog').close();}}));
$('#model-list').addEventListener('click',event=>{const button=event.target.closest('[data-model]');if(button)attempt(async()=>{const m=providers().find(p=>p.id===button.dataset.provider)?.models.find(m=>m.id===button.dataset.model);const effort=m.efforts.includes(profile().effort)?profile().effort:m.default_effort;await sendInput('set_inference_profile',{provider:button.dataset.provider,model:button.dataset.model,effort});$('#model-dialog').close();});});
$('#stop-inline').addEventListener('click',()=>attempt(()=>sendInput('cancel')));
for(const [id,approved] of [['approve',true],['deny',false]])$(`#${id}`).addEventListener('click',()=>attempt(async()=>{const request=session().pending_permissions?.[0];if(request)await sendInput('permission',{request_id:request.request_id,approved});}));
$('#attach').addEventListener('click',()=>$('#file-input').click());
$('#file-input').addEventListener('change',event=>{attempt(()=>addFiles([...event.target.files]));event.target.value='';});
$('#composer').addEventListener('dragover',event=>event.preventDefault());
$('#composer').addEventListener('drop',event=>{event.preventDefault();attempt(()=>addFiles([...event.dataTransfer.files]));});
$('#attachments').addEventListener('click',event=>{const button=event.target.closest('[data-remove-file]');if(button)attempt(()=>removeFile(Number(button.dataset.removeFile)));});
$('#open-settings').addEventListener('click',()=>openSettings());
$$('[data-close]').forEach(button=>button.addEventListener('click',()=>$(`#${button.dataset.close}`).close()));
$$('[data-settings]').forEach(button=>button.addEventListener('click',()=>{$$('[data-settings]').forEach(b=>b.classList.toggle('selected',b===button));state.settingsTab=button.dataset.settings;state.settingsProvider=null;state.settingsModel=null;renderSettings();}));
$('#settings-content').addEventListener('submit',event=>{const form=event.target.closest('[data-settings-form]');if(form){event.preventDefault();attempt(async()=>{const plan=settingsPlan(form);await setup(plan,plan.Apply?{provider:plan.Apply.name}:{});});}});
$('#settings-content').addEventListener('change',event=>attempt(async()=>{
  if(event.target.id==='theme'){document.body.classList.toggle('dark',event.target.value==='dark');try{localStorage.setItem('latch-theme',event.target.value);}catch{}}
  if(event.target.id==='safety-select')await sendInput('set_safety',event.target.value);
  if(event.target.id==='permission-select')await sendInput('set_permissions',event.target.value);
  if(event.target.name==='source') {
    const form=event.target.form;
    const provider=(metadata().setup_providers || []).find(p=>p.id===form.dataset.provider);
    const kind=(metadata().setup_catalog || []).find(k=>k.kind===(form.elements.provider_kind?.value || provider?.kind));
    const reference=provider?.credential_ref?.startsWith('env:')?provider.credential_ref:kind?.credential_label || '';
    const env=reference.startsWith('env:')?reference.slice(4):'';
    form.querySelector('[data-credential-value]').innerHTML=credentialInput(event.target.value,env);
  }
  if(event.target.name==='catalog_model')renderNewModel(event.target.form);
  if(event.target.id==='new-provider-kind'){state.newProviderKind=event.target.value;renderSettings();}
  if(event.target.id==='model-field'){state.modelField=event.target.value;renderSettings();}
  if(event.target.name==='field' && event.target.closest('[data-settings-form="provider-field"]')){const p=(metadata().setup_providers || []).find(p=>p.id===state.settingsProvider);event.target.form.elements.value.value=event.target.value==='BaseUrl'?p.base_url:p.display_name;}
}));
$('#settings-content').addEventListener('click',event=>attempt(async()=>{
  const target=event.target.closest('button');if(!target)return;
  if(target.dataset.openProvider){state.settingsProvider=target.dataset.openProvider;state.settingsModel=null;renderSettings();}
  if(target.hasAttribute('data-settings-back')){state.settingsProvider=null;state.settingsModel=null;renderSettings();}
  if(target.hasAttribute('data-model-back')){state.settingsModel=null;renderSettings();}
  if(target.dataset.editModel){const p=(metadata().setup_providers || []).find(p=>p.id===state.settingsProvider);if(!p)return;if(!p.models.some(m=>m.id===target.dataset.editModel)){await setup({AddCustomModel:{name:p.id,model:target.dataset.editModel,display_name:target.dataset.editModel}},{model:target.dataset.editModel});return;}state.settingsModel=target.dataset.editModel;state.modelField=p.models.find(m=>m.id===target.dataset.editModel).resolved?'DisplayName':'Transport';renderSettings();}
  if(target.dataset.discover){await sendInput('discover_models',{provider:target.dataset.discover});state.settingsMessage='Model refresh requested. This list updates automatically when availability arrives.';renderSettingsStatus();}
  if(target.dataset.newDefault)await setup({SetNewSessionDefault:{name:target.dataset.newDefault}});
  if(target.dataset.removeProvider)$('#provider-removal').hidden=false;
  if(target.hasAttribute('data-cancel-remove'))$('#provider-removal').hidden=true;
  if(target.dataset.confirmRemove)await setup({Remove:{name:target.dataset.confirmRemove}},{remove:true});
}));
document.addEventListener('click',event=>attempt(async()=>{
  if(!event.target.closest('.mode-control')){$('#mode-menu').hidden=true;$('#mode-button').setAttribute('aria-expanded','false');}
  const command=event.target.closest('[data-command]');if(command)await slash(command.dataset.command);
  const copy=event.target.closest('[data-copy]');if(copy){await navigator.clipboard.writeText(cellText(session().cells[Number(copy.dataset.copy)]));toast('Copied.');}
  const raw=event.target.closest('[data-raw]');if(raw){const body=raw.closest('.message').querySelector('.assistant-body');body.replaceChildren();const pre=document.createElement('pre');pre.className='tool-output';pre.textContent=cellText(session().cells[Number(raw.dataset.raw)]);body.append(pre);}
  const diff=event.target.closest('[data-cell-diff]');if(diff){const [type,data]=cellParts(session().cells[Number(diff.dataset.cellDiff)]);if(type==='Diff')showDiff(data.document.raw);else if(type==='Patch'){const f=data.files[Number(diff.dataset.file)];showDiff(f.preview || f.raw,f.path);}}
  const palette=event.target.closest('[data-palette-command]');if(palette){$('#commands-dialog').close();$('#prompt').value=palette.dataset.paletteCommand;renderComposer();$('#prompt').focus();}
}));
$('#commands-button').addEventListener('click',showCommands);
document.addEventListener('keydown',event=>{if((event.ctrlKey||event.metaKey)&&event.key.toLowerCase()==='k'){event.preventDefault();if(!$('dialog[open]'))attempt(newChat);}if(event.key==='Escape'&&!$('dialog[open]')){$('#mode-menu').hidden=true;setDetails(false);$('#app').classList.remove('mobile-sidebar-open');}});
const fragment=new URLSearchParams(location.hash.slice(1));const token=fragment.get('token');
if(token){history.replaceState(null,'',location.pathname+location.search);attempt(async()=>{await api('/api/auth',{method:'POST',body:{token}});await connect();});}
else attempt(connect);

setInterval(renderActivity,1000);
