export const state = {
  snapshot:null,
  sessions:[],
  managingSessions:false,
  selectedSessions:new Set(),
  detailTab:'task',
  settingsTab:'general',
  connected:false,
  selectedProvider:null,
  settingsProvider:null,
  settingsModel:null,
  settingsPending:null,
  settingsMessage:'',
  settingsError:false,
  pendingFiles:[],
  historyPosition:null,
  draftBeforeHistory:'',
  raw:false,
};
export function session() { return state.snapshot?.state || {}; }
export function metadata() { return session().metadata || {}; }
export function profile() {
  const header = metadata().header || {};
  return {...header, ...(metadata().inference || {}), mode:metadata().mode || session().sidebar?.session?.mode || 'WORK'};
}
export function providers() { return metadata().inference_catalog?.providers || []; }
export function model() { const p = profile(); return providers().find(v => v.id === p.provider_id)?.models.find(v => v.id === p.model); }
export function escapeHtml(value) { return String(value ?? '').replace(/[&<>"']/g,c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])); }
export function title(value) { return String(value || '').replace(/([a-z])([A-Z])/g,'$1 $2').replaceAll('_',' ').replace(/\b\w/g,c => c.toUpperCase()); }
export function mediaUrl(reference) { return `/api/sessions/${encodeURIComponent(session().session_id)}/media/${encodeURIComponent(reference.id)}`; }
export function cellParts(cell) { const entry = Object.entries(cell)[0]; return entry || ['Notice',{text:''}]; }
export function cellText(cell) {
  const [type,data] = cellParts(cell);
  if (data.text !== undefined) return data.text;
  if (type === 'Diff') return data.document.raw;
  if (type === 'Patch') return data.files.map(f => `${f.path}\n${f.preview || f.raw}`).join('\n\n');
  if (type === 'Exploration') return data.operations.map(o => `${o.label}\n${o.raw}`).join('\n\n');
  return data.raw || data.output || data.summary || JSON.stringify(data,null,2);
}
