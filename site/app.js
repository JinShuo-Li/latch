'use strict';
const siteRoot = new URL(document.currentScript.dataset.root, document.baseURI);

// Native details keeps navigation available when JavaScript is disabled.
const sidebar = document.querySelector('.sidebar');
const mobile = matchMedia('(max-width: 760px)');
function fitNavigation() { if (sidebar) sidebar.open = !mobile.matches; }
fitNavigation();
mobile.addEventListener('change', fitNavigation);

for (const pre of document.querySelectorAll('pre')) {
  const code = pre.querySelector('code');
  if (!code) continue;
  pre.tabIndex = 0;
  const language = [...code.classList].find(name => name.startsWith('language-'));
  pre.dataset.language = language ? language.slice(9) : 'text';
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'copy-button';
  button.textContent = 'Copy';
  button.setAttribute('aria-label', 'Copy code block');
  button.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(code.textContent.trimEnd());
      button.textContent = 'Copied';
      document.querySelector('#copy-feedback').textContent = 'Code copied to clipboard.';
    } catch {
      button.textContent = 'Select text';
      document.querySelector('#copy-feedback').textContent = 'Copy unavailable. Select the code to copy it.';
    }
    setTimeout(() => { button.textContent = 'Copy'; }, 2000);
  });
  pre.append(button);
}

const tabs = [...document.querySelectorAll('[role="tab"]')];
if (tabs.length) {
  document.querySelector('.interface-tabs').hidden = false;
  function selectTab(index, focus = false) {
    tabs.forEach((tab, i) => {
      const selected = i === index;
      tab.setAttribute('aria-selected', String(selected));
      tab.tabIndex = selected ? 0 : -1;
      const panel = document.getElementById(tab.getAttribute('aria-controls'));
      panel.hidden = !selected;
      panel.setAttribute('role', 'tabpanel');
      panel.setAttribute('aria-labelledby', tab.id);
      panel.tabIndex = 0;
    });
    if (focus) tabs[index].focus();
  }
  tabs.forEach((tab, i) => {
    tab.addEventListener('click', () => selectTab(i));
    tab.addEventListener('keydown', event => {
      if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
      event.preventDefault();
      selectTab(event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1 : (i + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length, true);
    });
  });
  selectTab(0);
}

const dialog = document.querySelector('#search-dialog');
const input = document.querySelector('#search-input');
const results = document.querySelector('#search-results');
const status = document.querySelector('#search-status');
let indexPromise;
let queryId = 0;
function loadIndex() {
  if (!indexPromise) {
    indexPromise = fetch(new URL('search-index.json', siteRoot)).then(response => {
      if (!response.ok) throw new Error('Search index unavailable');
      return response.json();
    }).catch(error => { indexPromise = null; throw error; });
  }
  return indexPromise;
}
async function search() {
  const request = ++queryId;
  results.replaceChildren();
  const query = input.value.trim().toLowerCase();
  if (!query) { status.textContent = 'Type to search. Results stay in your browser.'; return; }
  status.textContent = 'Searching…';
  try {
    const index = await loadIndex();
    if (request !== queryId) return;
    const words = query.split(/\s+/);
    const matches = index.map(entry => {
      const pageTitle = entry.title.toLowerCase();
      const title = (entry.title + ' ' + entry.section).toLowerCase();
      const text = (title + ' ' + entry.group + ' ' + entry.text).toLowerCase();
      return {entry, score: words.every(word => text.includes(word)) ? words.reduce((n, word) => n + (pageTitle.includes(word) ? 8 : title.includes(word) ? 5 : 1), 0) : 0};
    }).filter(match => match.score > 0).sort((a, b) => b.score - a.score);
    const unique = matches.filter((match, i) => matches.findIndex(other => other.entry.url === match.entry.url) === i);
    for (const {entry} of unique.slice(0, 12)) {
      const li = document.createElement('li');
      const link = document.createElement('a');
      link.href = new URL(entry.url, siteRoot);
      const group = document.createElement('small'); group.textContent = entry.group;
      const title = document.createElement('strong'); title.textContent = entry.title + (entry.section !== entry.title ? ' / ' + entry.section : '');
      const snippet = document.createElement('p');
      const position = Math.max(0, entry.text.toLowerCase().indexOf(words[0]) - 50);
      snippet.textContent = (position ? '…' : '') + entry.text.slice(position, position + 160) + (entry.text.length > position + 160 ? '…' : '');
      link.append(group, title, snippet); li.append(link); results.append(li);
    }
    status.textContent = unique.length ? `${unique.length} matching sections${unique.length > 12 ? '; showing the first 12' : ''}. Tab or press Arrow Down to navigate.` : 'No results. Try a command or a broader topic.';
  } catch {
    if (request === queryId) status.textContent = 'Search could not load. Try again or use the documentation sidebar.';
  }
}
if (typeof dialog.showModal === 'function') {
  const trigger = document.querySelector('[data-search]');
  trigger.hidden = false;
  trigger.addEventListener('click', () => { dialog.showModal(); input.focus(); search(); });
  document.querySelector('#search-close').addEventListener('click', () => dialog.close());
  dialog.addEventListener('click', event => { if (event.target === dialog && (event.clientX < dialog.getBoundingClientRect().left || event.clientX > dialog.getBoundingClientRect().right || event.clientY < dialog.getBoundingClientRect().top || event.clientY > dialog.getBoundingClientRect().bottom)) dialog.close(); });
  input.addEventListener('input', search);
  dialog.addEventListener('keydown', event => {
    if (!['ArrowDown', 'ArrowUp'].includes(event.key)) return;
    const links = [...results.querySelectorAll('a')];
    if (!links.length) return;
    event.preventDefault();
    const current = links.indexOf(document.activeElement);
    if (event.key === 'ArrowDown') links[(current + 1) % links.length].focus();
    else if (current <= 0) input.focus();
    else links[current - 1].focus();
  });
  document.addEventListener('keydown', event => {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
      event.preventDefault();
      if (dialog.open) dialog.close(); else trigger.click();
    }
  });
}
