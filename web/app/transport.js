// All URLs are relative so an SSH tunnel may use a different local port.
export async function api(path, options = {}) {
  const headers = new Headers(options.headers || {});
  let body = options.body;
  if (body !== undefined && !(body instanceof Blob) && !(body instanceof ArrayBuffer)) {
    headers.set('Content-Type', 'application/json');
    body = JSON.stringify(body);
  }
  const response = await fetch(path, {...options, headers, body, credentials:'same-origin', cache:'no-store'});
  const data = await response.json().catch(() => ({}));
  if (!response.ok) {
    const error = new Error(data.error || `Request failed (${response.status})`);
    error.status = response.status;
    throw error;
  }
  return data;
}

export function subscribe(onSnapshot, onConnection, onError) {
  const events = new EventSource('/api/events');
  let refreshing = false;
  let dirty = false;
  let timer;
  let closed = false;
  const refresh = async () => {
    if (closed) return;
    if (refreshing) { dirty = true; return; }
    refreshing = true;
    try { onSnapshot(await api('/api/bootstrap')); }
    catch (error) { onError(error); }
    finally {
      refreshing = false;
      if (dirty && !closed) { dirty = false; clearTimeout(timer); timer = setTimeout(() => { timer = undefined; refresh(); }, 100); }
    }
  };
  events.addEventListener('snapshot', event => {
    try { onSnapshot(JSON.parse(event.data)); onConnection(true); }
    catch { onError(new Error('Could not read session state. Reconnect to Latch.')); }
  });
  events.addEventListener('changed', () => {
    if (refreshing) dirty = true;
    else if (!timer) timer = setTimeout(() => { timer = undefined; refresh(); },100);
  });
  events.onopen = () => onConnection(true);
  events.onerror = () => {
    onConnection(false);
    // Detect an expired cookie/server restart instead of retrying invisibly.
    clearTimeout(timer); timer = setTimeout(() => { timer = undefined; refresh(); },1000);
  };
  return () => { closed = true; clearTimeout(timer); events.close(); };
}
