import { state, dom } from './state.js';

const loggedInstances = new Set();
let activeLogFilter = 'all';

function fmtTime() {
  const d = new Date();
  return `${String(d.getHours()).padStart(2,'0')}:${String(d.getMinutes()).padStart(2,'0')}:${String(d.getSeconds()).padStart(2,'0')}`;
}

export function appendLog(source, text, instanceId, instanceName) {
  const line = document.createElement('div');
  line.className = `log-line ${source}`;
  if (instanceId) line.dataset.instance = instanceId;

  // Timestamp
  const ts = document.createElement('span');
  ts.className = 'log-ts';
  ts.textContent = fmtTime();
  line.appendChild(ts);

  // Instance tag (hidden by CSS unless .multi)
  if (instanceName) {
    const tag = document.createElement('span');
    tag.className = 'log-tag';
    tag.textContent = instanceName;
    line.appendChild(tag);

    if (!loggedInstances.has(instanceId)) {
      loggedInstances.add(instanceId);
      if (loggedInstances.size > 1) dom.logLines?.classList.add('multi');
      syncLogFilter();
    }
  }

  line.appendChild(document.createTextNode(text));

  // Apply active filter
  if (activeLogFilter !== 'all' && instanceId && instanceId !== activeLogFilter) {
    line.classList.add('log-filtered');
  }

  dom.logLines?.appendChild(line);
  state.logLines++;
  if (dom.logCount) dom.logCount.textContent = `${state.logLines} line${state.logLines !== 1 ? 's' : ''}`;
  if (dom.logContent) dom.logContent.scrollTop = dom.logContent.scrollHeight;
}

export function setLogFilter(instanceId) {
  activeLogFilter = instanceId || 'all';
  if (!dom.logLines) return;
  const lines = dom.logLines.querySelectorAll('.log-line');
  for (const line of lines) {
    const lid = line.dataset.instance;
    if (activeLogFilter === 'all' || !lid || lid === activeLogFilter) {
      line.classList.remove('log-filtered');
    } else {
      line.classList.add('log-filtered');
    }
  }
  if (dom.logContent) dom.logContent.scrollTop = dom.logContent.scrollHeight;
}

function syncLogFilter() {
  if (!dom.logFilter) return;
  // Rebuild filter options
  const current = dom.logFilter.value;
  dom.logFilter.innerHTML = '<option value="all">All instances</option>';
  for (const id of loggedInstances) {
    const inst = state.instances.find(i => i.id === id);
    const name = inst?.name || id.slice(0, 8);
    const opt = document.createElement('option');
    opt.value = id;
    opt.textContent = name;
    dom.logFilter.appendChild(opt);
  }
  dom.logFilter.value = current || 'all';
  dom.logFilter.classList.toggle('hidden', loggedInstances.size < 2);
}

export function showError(msg) {
  appendLog('stderr', `ERROR: ${msg}`);
  dom.logPanel?.classList.add('expanded');
}

export function esc(s) {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}

export function fmtMem(gb) { return gb === Math.floor(gb) ? `${gb}\u00A0GB` : `${gb.toFixed(1)}\u00A0GB`; }

export function formatBytes(bytes) {
  if (bytes < 1024) return bytes + ' B';
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
  if (bytes < 1024 * 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
  return (bytes / (1024 * 1024 * 1024)).toFixed(2) + ' GB';
}

export function formatRelativeTime(date) {
  const now = new Date();
  const diff = now - date;
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  if (days < 7) return `${days}d ago`;
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' }).format(date);
}

export function getMemoryRecommendation(totalGB) {
  if (totalGB <= 4) return { rec: 2, text: 'Low RAM — 2 GB recommended' };
  if (totalGB <= 8) return { rec: 4, text: '4 GB recommended' };
  if (totalGB <= 16) return { rec: 6, text: '6 GB recommended' };
  return { rec: 8, text: '8 GB recommended' };
}

export function updateMemoryRecText(val, totalGB) {
  if (!totalGB || !dom.memoryRec) return;
  dom.memoryRec.textContent = val < 2 ? '(low — may lag)' : val > totalGB * 0.75 ? '(high — leave room for OS)' : '';
}

export function setPage(page) {
  state.currentPage = page;
  dom.launcherView?.classList.toggle('hidden', page !== 'launcher');
  dom.settingsView?.classList.toggle('hidden', page !== 'settings');
  dom.sidebarLauncherPanel?.classList.toggle('hidden', page !== 'launcher');
  dom.sidebarSettingsPanel?.classList.toggle('hidden', page !== 'settings');
  dom.settingsBtn?.classList.toggle('active', page === 'settings');
}

export function toggleShortcutHints(show) {
  document.body.classList.toggle('show-shortcuts', show);
}
