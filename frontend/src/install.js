import { state, dom, API } from './state.js';
import { api } from './api.js';
import { showError } from './utils.js';
import { renderInstanceList } from './sidebar.js';
import { renderSelectedInstance, resetInstallUI, refreshSelectedInstanceActionState } from './instance.js';

function show(el) { if (el) el.classList.remove('action-hidden'); }

export function installVersion(explicitTarget) {
  const target = explicitTarget || dom.installBtn?.dataset.installTarget || state.selectedInstance?.version_id;
  if (!target) return;

  // Already active or queued for this version — skip
  if (state.activeInstall?.versionId === target) return;
  if (state.installQueue.some(q => q.versionId === target)) return;

  state.installQueue.push({ versionId: target });
  updateSidebarProgress(target, 0);
  refreshSelectedInstanceActionState();

  if (!state.activeInstall) processNextInstall();
}

async function processNextInstall() {
  if (state.activeInstall) return;
  const next = state.installQueue.shift();
  if (!next) return;

  state.activeInstall = { versionId: next.versionId, pct: 0, label: 'Starting download...' };
  updateSidebarProgress(next.versionId, 0);
  refreshSelectedInstanceActionState();

  try {
    const res = await api('POST', '/install', { version_id: next.versionId });
    if (res.error) { showError(res.error); onInstallDone(); return; }
    connectInstallSSE(res.install_id, next.versionId);
  } catch (err) {
    showError('Install failed: ' + err.message);
    onInstallDone();
  }
}

function connectInstallSSE(installId, versionId) {
  if (state.installEventSource) state.installEventSource.close();
  const es = new EventSource(`${API}/install/${installId}/events`);
  state.installEventSource = es;
  const startTime = Date.now();

  es.addEventListener('progress', (e) => {
    const d = JSON.parse(e.data);
    let pct = 0;
    let label = '';

    if (d.phase === 'version_json') {
      pct = 2; label = 'Fetching version info...';
    } else if (d.phase === 'client_jar') {
      pct = 7; label = 'Downloading game JAR...';
    } else if (d.phase === 'libraries') {
      const libPct = d.total > 0 ? d.current / d.total : 0;
      pct = 7 + Math.round(libPct * 13);
      label = `Libraries (${d.current}/${d.total})`;
    } else if (d.phase === 'asset_index') {
      pct = 21; label = 'Downloading asset index...';
    } else if (d.phase === 'assets') {
      const assetPct = d.total > 0 ? d.current / d.total : 0;
      pct = 21 + Math.round(assetPct * 72);
      label = `Assets (${d.current}/${d.total})`;
    } else if (d.phase === 'log_config') {
      pct = 94; label = 'Downloading log config...';
    } else if (d.phase === 'done') {
      pct = 100; label = 'Complete!';
    } else if (d.phase === 'error') {
      showError(d.error); updateSidebarProgress(versionId, -1); onInstallDone(); return;
    }

    if (pct > 5 && pct < 100) {
      const elapsed = (Date.now() - startTime) / 1000;
      const remaining = (elapsed / pct) * (100 - pct);
      if (remaining < 60) label += ` — ~${Math.ceil(remaining)}s left`;
      else label += ` — ~${Math.ceil(remaining / 60)}m left`;
    }

    if (state.activeInstall) {
      state.activeInstall.pct = pct;
      state.activeInstall.label = label;
    }
    updateDetailProgress(versionId, pct, label);
    updateSidebarProgress(versionId, pct);

    if (d.done) onInstallDone();
  });
  es.onerror = () => { if (state.activeInstall) { updateSidebarProgress(versionId, -1); onInstallDone(); } };
}

function updateDetailProgress(versionId, pct, label) {
  const inst = state.selectedInstance;
  if (!inst) return;
  const v = state.versions.find(v => v.id === inst.version_id);
  const target = v?.needs_install || v?.id || inst.version_id;
  if (target !== versionId) return;

  show(dom.installArea);
  show(dom.installProgress);
  if (dom.progressFill) dom.progressFill.style.width = pct + '%';
  if (dom.progressText) dom.progressText.textContent = label;
}

function updateSidebarProgress(versionId, pct) {
  if (!versionId) return;
  for (const inst of state.instances) {
    const v = state.versions.find(v => v.id === inst.version_id);
    const target = v?.needs_install || v?.id || inst.version_id;
    if (target !== versionId) continue;

    const el = dom.versionList?.querySelector(`.version-item[data-id="${CSS.escape(inst.id)}"]`);
    if (!el) continue;
    let bar = el.querySelector('.version-install-bar');
    if (pct < 0) { if (bar) bar.remove(); continue; }
    if (!bar) {
      bar = document.createElement('div');
      bar.className = 'version-install-bar';
      bar.innerHTML = '<div class="version-install-fill"></div>';
      el.appendChild(bar);
    }
    const fill = bar.querySelector('.version-install-fill');
    if (fill) fill.style.width = pct + '%';
    if (pct >= 100) setTimeout(() => bar.remove(), 1500);
  }
}

async function onInstallDone() {
  state.activeInstall = null;
  if (state.installEventSource) { state.installEventSource.close(); state.installEventSource = null; }

  try {
    const res = await api('GET', '/versions');
    state.versions = res.versions || [];
    if (state.catalog?.versions) {
      const installed = new Set(state.versions.filter(v => v.launchable).map(v => v.id));
      state.catalog.versions.forEach(v => { v.installed = installed.has(v.id); });
    }
    renderInstanceList();
    if (state.selectedInstance) renderSelectedInstance();
  } catch {}

  processNextInstall();
}
