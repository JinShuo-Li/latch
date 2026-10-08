import {state,session,metadata,escapeHtml as e,title} from './state.js';
import {icon} from './icons.js';
const $ = selector => document.querySelector(selector);
const efforts = ['none','minimal','low','medium','high','xhigh','max'];
const choices = (values,current) => values.map(v=>`<option value="${e(v)}" ${v===current?'selected':''}>${e(title(v))}</option>`).join('');
const input = (label,name,value='',type='text',extra='') => `<label class="field-label">${e(label)}<input class="form-input" name="${name}" type="${type}" value="${e(value ?? '')}" ${extra}></label>`;
const save = label => `<button type="submit" class="primary-button">${label || 'Save change'}</button>`;
const providers = () => metadata().setup_providers || [];
function credentialForm(name) {
  return `<form data-settings-form="credential" data-provider="${e(name)}"><h3>Replace credential</h3><p class="settings-note">The current value is never displayed. A new secret is stored by Latch on the server.</p><label class="field-label">Credential source<select class="form-input" name="source"><option value="Env">Environment variable</option><option value="Secret">Store a new API key</option></select></label>${input('Environment variable name or new API key','credential','','password','required autocomplete="off"')}${save('Save credential')}</form>`;
}
export function renderSettings() {
  document.querySelectorAll('[data-settings]').forEach(b=>b.classList.toggle('selected',b.dataset.settings===state.settingsTab));
  const root = $('#settings-content');
  if (state.settingsTab === 'general') {
    root.innerHTML = `<div class="settings-row"><div><label for="theme">Appearance</label><small>A comfortable place to focus.</small></div><select id="theme">${choices(['dark','light'],document.body.classList.contains('dark')?'dark':'light')}</select></div><div class="settings-row"><div><strong>Language</strong><small>Latch’s default interface language.</small></div><span class="fixed-value">English</span></div><div class="settings-row"><div><strong>Workspace</strong><small>Fixed to the startup directory.</small></div><span class="fixed-value">${e(state.snapshot?.workspace)}</span></div><div class="settings-row"><div><strong>Keyboard shortcuts</strong><small>Enter to send · Shift + Enter for a new line<br>Ctrl / ⌘ + K for a new conversation<br>Up / Down at an input boundary for prompt history<br>Escape to close panels</small></div></div><button class="secondary-button" data-command="/quit">Stop Web server</button>`;
  } else if (state.settingsTab === 'safety') {
    root.innerHTML = `<div class="settings-row"><div><label for="safety-select">Safety profile</label><small>Controls which operations are permitted.</small></div><select id="safety-select">${choices(['strict','standard','autonomous'],metadata().safety || 'standard')}</select></div><div class="settings-row"><div><label for="permission-select">Permission resolver</label><small>How approval requests are resolved.</small></div><select id="permission-select">${choices(['auto','human','ai'],metadata().permissions || 'human')}</select></div><p class="settings-note">Ask and Plan remain read-only. Every command still runs inside the mandatory sandbox.</p>`;
  } else renderProviderPage();
}
function renderProviderPage() {
  const root = $('#settings-content');
  const provider = providers().find(p=>p.id===state.settingsProvider);
  const model = provider?.models.find(m=>m.id===state.settingsModel);
  if (model) {renderModelPage(provider,model);return;}
  if (state.settingsProvider === '__new') {renderNewProvider();return;}
  if (!provider) {
    root.innerHTML = `${providers().map(p=>`<button class="provider-card provider-choice" data-open-provider="${e(p.id)}"><span class="status-pill">${e(title(p.status))}</span><strong>${e(p.display_name)}</strong><p>${e(p.default_model || 'No default model')} · ${p.model_count} models<br>${e(p.credential_ref)}</p></button>`).join('') || '<p class="settings-note">Add a provider to start using Latch.</p>'}<button class="secondary-button" data-open-provider="__new">${icon('plus')} Add provider</button>`;
    return;
  }
  const discoveries = metadata().setup_models?.provider === provider.id ? metadata().setup_models.ids : [];
  const models = [...provider.models];
  for (const id of discoveries) if (!models.some(m=>m.id===id)) models.push({id,display_name:id,enabled:false,resolved:false});
  root.innerHTML = `<button class="back-button" data-settings-back>← Providers</button><h3>${e(provider.display_name)}</h3><p class="settings-note">${e(provider.kind)} · ${e(provider.credential_ref)}<br>${e(title(provider.status))}</p><form data-settings-form="provider-field" data-provider="${e(provider.id)}"><label class="field-label">Field<select class="form-input" name="field"><option value="BaseUrl">Base URL</option><option value="DisplayName">Display name</option></select></label>${input('Value (empty restores the built-in default)','value',provider.base_url)}${save()}</form><form data-settings-form="provider-default" data-provider="${e(provider.id)}"><label class="field-label">Default model for this provider<select class="form-input" name="model">${provider.available_models.map(id=>`<option ${id===provider.default_model?'selected':''}>${e(id)}</option>`).join('')}</select></label>${save('Save model default')}</form>${credentialForm(provider.id)}<form data-settings-form="enabled-models" data-provider="${e(provider.id)}"><h3>Enabled models</h3><div class="model-configuration-list">${models.map(m=>`<div class="model-configuration-row"><label><input type="checkbox" name="models" value="${e(m.id)}" ${m.enabled?'checked':''}>${e(m.display_name || m.id)}</label><button type="button" class="text-button" data-edit-model="${e(m.id)}">${m.resolved?'Advanced':'Resolve'}</button></div>`).join('')}</div>${save('Save enabled models')}<button type="button" class="secondary-button" data-discover="${e(provider.id)}">Refresh availability</button></form><form data-settings-form="custom-model" data-provider="${e(provider.id)}"><h3>Add a custom model</h3>${input('Model ID','model','','text','required')}${input('Display name','display_name')}${save('Add model')}</form><div class="detail-actions"><button class="secondary-button" data-new-default="${e(provider.id)}">Use for new sessions</button><button class="secondary-button danger-button" data-remove-provider="${e(provider.id)}">Remove provider</button></div>`;
}
function renderNewProvider() {
  const catalog = metadata().setup_catalog || [];
  const selected = state.newProviderKind || catalog[0]?.kind;
  const kind = catalog.find(k=>k.kind===selected);
  $('#settings-content').innerHTML = `<button class="back-button" data-settings-back>← Providers</button><form data-settings-form="new-provider"><h3>Add a provider</h3><label class="field-label">Provider kind<select class="form-input" id="new-provider-kind" name="provider_kind">${catalog.map(k=>`<option value="${e(k.kind)}" ${k.kind===selected?'selected':''}>${e(k.label)}</option>`).join('')}</select></label>${input('Instance name','name',kind?.kind,'text','required')}${input('Base URL','base_url',kind?.default_base_url,'url',kind?.requires_base_url?'required':'')}${input('Default model ID','model',kind?.default_model,'text','required list="setup-models"')}<datalist id="setup-models">${(kind?.models||[]).map(m=>`<option value="${e(m.id)}">${e(m.display_name)}</option>`).join('')}</datalist><label class="field-label">Credential source<select class="form-input" name="source"><option value="Env">Environment variable</option><option value="Secret">Store a new API key</option></select></label>${input('Environment variable name or new API key','credential','','password','required autocomplete="off"')}<label class="field-label">Custom model transport (if not in the catalog)<select class="form-input" name="transport"><option value="">Built-in catalog</option>${choices(['chat_completions','responses','anthropic_messages','gemini'],'')}</select></label>${save('Configure provider')}</form>`;
}
export function renderModelPage(provider,model) {
  const fields = ['DisplayName','Transport','ContextWindow','ReasoningReplay','AdaptiveThinking','GeminiThinking','InputModalities','Aliases','Efforts','EffortMap','Pricing','Reset'];
  state.modelField ||= 'DisplayName';
  $('#settings-content').innerHTML = `<button class="back-button" data-model-back>← ${e(provider.display_name)}</button><h3>${e(model.display_name || model.id)}</h3><p class="settings-note">${e(model.id)} · ${e(model.transport || 'Transport unresolved')}<br>${model.context_window_tokens?`${model.context_window_tokens.toLocaleString('en')} token context`:'Context window unknown'}</p><form data-settings-form="model-field" data-provider="${e(provider.id)}" data-model="${e(model.id)}"><label class="field-label">Advanced setting<select class="form-input" id="model-field" name="field">${fields.map(f=>`<option ${f===state.modelField?'selected':''}>${f}</option>`).join('')}</select></label><div id="model-field-value">${fieldEditor(state.modelField,model)}</div>${save(state.modelField==='Reset'?'Reset model settings':'Save this setting')}</form>`;
}
function fieldEditor(field,m) {
  if (field === 'DisplayName') return input('Display name (empty restores default)','value',m.display_name);
  if (field === 'Transport') return `<label class="field-label">Transport<select class="form-input" name="value"><option value="">Provider default</option>${choices(['chat_completions','responses','anthropic_messages','gemini'],m.transport_configured?m.transport:'')}</select></label>`;
  if (field === 'ContextWindow') return input('Context window in tokens (empty restores default)','value',m.context_window_tokens,'number','min="1"');
  if (field === 'ReasoningReplay') return `<label class="field-label">Replay policy<select class="form-input" name="value"><option value="">Provider default</option>${choices(['replay','omit'],m.reasoning_replay || '')}</select></label>`;
  if (field === 'AdaptiveThinking') return `<label class="field-label">Adaptive thinking<select class="form-input" name="value"><option value="">Provider default</option>${choices(['true','false'],m.adaptive_thinking_configured?String(m.adaptive_thinking):'')}</select></label>`;
  if (field === 'InputModalities') return ['text','image'].map(v=>`<label class="checkbox-row"><input type="checkbox" name="values" value="${v}" ${m.input_modalities.includes(v)?'checked':''}>${title(v)}</label>`).join('');
  if (field === 'Aliases') return input('Aliases, separated by commas','value',m.aliases.join(', '));
  if (field === 'Efforts') return `<p class="settings-note">Changing exposed efforts may reset an incompatible effort mapping.</p>${efforts.map(v=>`<label class="checkbox-row"><input type="checkbox" name="values" value="${v}" ${m.efforts.includes(v)?'checked':''}>${title(v)}</label>`).join('')}<label class="field-label">Default effort<select class="form-input" name="default_effort">${choices(['provider_default',...efforts],m.default_effort)}</select></label>`;
  if (field === 'EffortMap') return `<p class="settings-note">Map every exposed level, or choose Provider default for all levels to clear the mapping.</p>${m.efforts.map(v=>{const value=m.effort_map[v];const type=typeof value==='string'?value:Object.keys(value||{})[0] || 'Automatic';return `<div class="effort-map-row"><strong>${title(v)}</strong><select class="form-input" name="map_${v}">${choices(['Automatic','Value','Budget','Disabled'],type)}</select><input class="form-input" name="value_${v}" aria-label="${title(v)} mapped value" value="${e(value?.Value ?? value?.Budget ?? '')}" placeholder="Value or token budget"></div>`;}).join('')}`;
  if (field === 'Pricing') return `${input('Currency','currency',m.pricing?.currency || 'USD')}${['input_per_million','output_per_million','cache_read_per_million','cache_write_per_million'].map(v=>input(title(v),'price_'+v,m.pricing?.[v],'number','min="0" step="any"')).join('')}<label class="checkbox-row"><input type="checkbox" name="clear_pricing">Clear pricing override</label>`;
  if (field === 'GeminiThinking') return `<label class="field-label">Thinking capability<select class="form-input" name="thinking"><option value="">Provider default</option><option value="Levels">Named levels</option><option value="Budget">Token budget</option></select></label>${input('Levels, separated by commas','levels','low, high')}${input('Off level, if supported','off')}<label class="checkbox-row"><input type="checkbox" name="zero_allowed">Budget can be zero</label>`;
  return '<p class="settings-note">Clear this model’s overrides. A custom model will be removed; built-in models return to catalog defaults.</p>';
}
export function settingsPlan(form) {
  const data = new FormData(form); const name = form.dataset.provider;
  const value = key => String(data.get(key) || '').trim();
  const credential = () => ({[value('source')]:value('credential')});
  switch(form.dataset.settingsForm) {
    case 'credential':return {SetCredential:{name,credential:credential()}};
    case 'provider-field':return {SetProviderField:{name,field:{[value('field')]:value('value') || null}}};
    case 'provider-default':return {SetProviderDefault:{name,model:value('model')}};
    case 'enabled-models':return {SetEnabledModels:{name,models:data.getAll('models')}};
    case 'custom-model':return {AddCustomModel:{name,model:value('model'),display_name:value('display_name')}};
    case 'new-provider':return {Apply:{name:value('name'),provider_kind:value('provider_kind'),base_url:value('base_url') || null,credential:credential(),model:value('model'),enabled_models:null,custom_model_display_name:null,custom_transport:value('transport') || null,effort:'provider_default'}};
    case 'model-field': {
      const field = value('field'); let edited;
      if (field === 'Reset') edited = 'Reset';
      else {
        let v = value('value') || null;
        if (field === 'ContextWindow') v = v===null?null:Number(v);
        if (field === 'AdaptiveThinking') v = v===null?null:v==='true';
        if (field === 'InputModalities') v = data.getAll('values');
        if (field === 'Aliases') v = value('value').split(',').map(v=>v.trim()).filter(Boolean);
        if (field === 'Efforts') v = {efforts:data.getAll('values'),default_effort:value('default_effort') || null};
        if (field === 'EffortMap') {
          v={}; const m=providers().find(p=>p.id===name)?.models.find(m=>m.id===form.dataset.model);
          for (const level of m?.efforts || []) {const type=value('map_'+level);if(type==='Automatic')continue;v[level]=type==='Disabled'?'Disabled':{[type]:type==='Budget'?Number(value('value_'+level)):value('value_'+level)};}
        }
        if (field === 'Pricing') v = data.has('clear_pricing')?null:{currency:value('currency'),...Object.fromEntries(['input_per_million','output_per_million','cache_read_per_million','cache_write_per_million'].map(k=>[k,value('price_'+k)?Number(value('price_'+k)):null]))};
        if (field === 'GeminiThinking') v = value('thinking')==='Levels'?{Levels:{levels:value('levels').split(',').map(v=>v.trim()).filter(Boolean),off:value('off') || null}}:value('thinking')==='Budget'?{Budget:{zero_allowed:data.has('zero_allowed')}}:null;
        edited = {[field]:v};
      }
      return {SetModelField:{name,model:form.dataset.model,field:edited}};
    }
    default:throw new Error('Unknown settings form.');
  }
}
