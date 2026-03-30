import { state, dom, local, saveLocalState, API } from './state.js';
import { Sound } from './sound.js';
import { esc, setPage, parseVersionDisplay } from './utils.js';
import { selectInstance, selectVersion, renderSelectedVersion, renderSelectedInstance } from './instance.js';
import { showInstanceContextMenu, showContextMenu } from './context-menu.js';

// --- Instance List ---
// Exact copy of renderInstanceList from lines 826-923
export function renderInstanceList() {
  if (!dom.versionList) return;
  const instances = filterInstances(state.instances);

  if (state.instances.length === 0) {
    dom.versionList.innerHTML = `<div class="loading-placeholder"><span>No instances</span></div>`;
    if (dom.emptyTitle) dom.emptyTitle.textContent = 'No instances yet';
    if (dom.emptySub) dom.emptySub.textContent = 'Create an instance to get started';
    dom.emptyAddBtn?.classList.remove('hidden');
    return;
  }

  if (!state.selectedInstance) {
    if (dom.emptyTitle) dom.emptyTitle.textContent = 'Select an instance';
    if (dom.emptySub) dom.emptySub.textContent = 'Choose an instance from the sidebar to launch';
    dom.emptyAddBtn?.classList.remove('hidden');
  } else {
    dom.emptyAddBtn?.classList.add('hidden');
  }

  if (instances.length === 0) {
    dom.versionList.innerHTML = `<div class="loading-placeholder"><span>No matching instances</span></div>`;
    return;
  }

  // Group by version type
  const versionMap = {};
  for (const v of state.versions) versionMap[v.id] = v;

  const groups = { release: [], snapshot: [], modded: [], other: [] };
  for (const inst of instances) {
    const v = versionMap[inst.version_id];
    if (v?.inherits_from) groups.modded.push(inst);
    else if (v?.type === 'release') groups.release.push(inst);
    else if (v?.type === 'snapshot') groups.snapshot.push(inst);
    else groups.other.push(inst);
  }

  let html = '';
  const chevron = `<svg class="version-group-chevron" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><polyline points="6 9 12 15 18 9"/></svg>`;

  const renderGroup = (key, label, items) => {
    if (!items.length) return;
    const collapsed = local.collapsedGroups[key];
    html += `<div class="version-group-label${collapsed ? ' collapsed' : ''}" data-group="${key}">${chevron}${label} <span style="opacity:.4;font-weight:400;margin-left:2px">${items.length}</span></div>`;
    html += `<div class="version-group-items${collapsed ? ' collapsed' : ''}" data-group-items="${key}">`;
    items.forEach((inst, i) => {
      const v = versionMap[inst.version_id];
      const isModded = !!v?.inherits_from;
      const bc = isModded ? 'badge-modded' : v?.type === 'release' ? 'badge-release' : v?.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
      const bt = isModded ? 'MOD' : v?.type === 'release' ? 'REL' : v?.type === 'snapshot' ? 'SNAP' : v?.type?.toUpperCase()?.slice(0, 4) || '?';
      const isRunning = !!state.runningSessions[inst.id];
      const dc = isRunning ? 'running' : v?.launchable ? 'ok' : 'missing';
      const sel = state.selectedInstance?.id === inst.id ? 'selected' : '';
      const rc = isRunning ? 'is-running' : '';
      const dim = v?.launchable ? '' : 'dimmed';
      const iTarget = v?.needs_install || v?.id || inst.version_id;
      const iPct = state.activeInstall?.versionId === iTarget ? (state.activeInstall.pct || 0) : state.installQueue.some(q => q.versionId === iTarget) ? 0 : -1;
      const iBar = iPct >= 0 ? `<div class="version-install-bar"><div class="version-install-fill" style="width:${iPct}%"></div></div>` : '';
      const pd = parseVersionDisplay(inst.version_id, v, state.versions);
      const sub = pd.hint ? `${esc(pd.name)} <span class="version-hint">${esc(pd.hint)}</span>` : esc(pd.name);
      html += `<button type="button" class="version-item ${dim} ${sel} ${rc}" data-id="${inst.id}" aria-pressed="${sel ? 'true' : 'false'}" aria-label="Select instance ${esc(inst.name)}" style="animation-delay:${i * 15}ms"><div class="version-dot ${dc}"></div><span class="version-name">${esc(inst.name)}</span><span class="version-sub">${sub}</span>${isRunning ? '<span class="version-running-tag">LIVE</span>' : ''}<span class="version-badge ${bc}">${bt}</span>${iBar}</button>`;
    });
    html += `</div>`;
  };

  renderGroup('release', 'Releases', groups.release);
  renderGroup('modded', 'Modded', groups.modded);
  renderGroup('snapshot', 'Snapshots', groups.snapshot);
  renderGroup('other', 'Other', groups.other);
  dom.versionList.innerHTML = html;

  dom.versionList.querySelectorAll('.version-item').forEach(el => {
    const inst = state.instances.find(i => i.id === el.dataset.id);
    el.addEventListener('focus', () => {
      if (!inst || state.selectedInstance?.id === inst.id) return;
      selectInstance(inst, { silent: true });
    });
    el.addEventListener('click', (e) => {
      if (e.button !== 0) return;
      if (inst) selectInstance(inst);
    });
    el.addEventListener('contextmenu', (e) => {
      if (inst) {
        e.preventDefault();
        e.stopPropagation();
        selectInstance(inst, { silent: true });
        showInstanceContextMenu(e, inst);
      }
    });
  });

  dom.versionList.querySelectorAll('.version-group-label').forEach(label => {
    label.addEventListener('click', () => {
      const key = label.dataset.group;
      local.collapsedGroups[key] = !local.collapsedGroups[key];
      saveLocalState();
      label.classList.toggle('collapsed');
      const items = dom.versionList.querySelector(`[data-group-items="${key}"]`);
      if (items) items.classList.toggle('collapsed');
    });
  });
}

// Exact copy of filterInstances from lines 925-936
export function filterInstances(instances) {
  let list = instances;
  const versionMap = {};
  for (const v of state.versions) versionMap[v.id] = v;

  if (state.filter === 'release') list = list.filter(inst => { const v = versionMap[inst.version_id]; return v?.type === 'release' && !v?.inherits_from; });
  else if (state.filter === 'snapshot') list = list.filter(inst => { const v = versionMap[inst.version_id]; return v?.type === 'snapshot' && !v?.inherits_from; });
  else if (state.filter === 'modded') list = list.filter(inst => { const v = versionMap[inst.version_id]; return !!v?.inherits_from; });

  if (state.search) { const q = state.search.toLowerCase(); list = list.filter(inst => inst.name.toLowerCase().includes(q) || inst.version_id.toLowerCase().includes(q)); }
  return list;
}

// --- Version List (legacy) ---
// Exact copy of renderVersionList from lines 942-1037
export function renderVersionList() {
  if (!dom.versionList) return;
  const filtered = filterVersions(state.versions);

  if (state.versions.length === 0) {
    dom.versionList.innerHTML = `<div class="loading-placeholder"><span>No versions installed</span></div>`;
    if (dom.emptyTitle) dom.emptyTitle.textContent = 'No versions installed';
    if (dom.emptySub) dom.emptySub.textContent = 'Add a Minecraft version to get started';
    dom.emptyAddBtn?.classList.remove('hidden');
    return;
  }

  if (!state.selectedVersion) {
    if (dom.emptyTitle) dom.emptyTitle.textContent = 'Select a version';
    if (dom.emptySub) dom.emptySub.textContent = 'Choose a Minecraft version from the sidebar to launch';
    dom.emptyAddBtn?.classList.remove('hidden');
  } else {
    dom.emptyAddBtn?.classList.add('hidden');
  }

  if (filtered.length === 0) {
    dom.versionList.innerHTML = `<div class="loading-placeholder"><span>No matching versions</span></div>`;
    return;
  }

  const groups = { release: [], snapshot: [], modded: [], other: [] };
  for (const v of filtered) {
    if (v.inherits_from) groups.modded.push(v);
    else if (v.type === 'release') groups.release.push(v);
    else if (v.type === 'snapshot') groups.snapshot.push(v);
    else groups.other.push(v);
  }

  let html = '';
  const chevron = `<svg class="version-group-chevron" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><polyline points="6 9 12 15 18 9"/></svg>`;

  const renderGroup = (key, label, versions) => {
    if (!versions.length) return;
    const collapsed = local.collapsedGroups[key];
    html += `<div class="version-group-label${collapsed ? ' collapsed' : ''}" data-group="${key}">${chevron}${label} <span style="opacity:.4;font-weight:400;margin-left:2px">${versions.length}</span></div>`;
    html += `<div class="version-group-items${collapsed ? ' collapsed' : ''}" data-group-items="${key}">`;
    versions.forEach((v, i) => {
      const isModded = !!v.inherits_from;
      const bc = isModded ? 'badge-modded' : v.type === 'release' ? 'badge-release' : v.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
      const bt = isModded ? 'MOD' : v.type === 'release' ? 'REL' : v.type === 'snapshot' ? 'SNAP' : v.type?.toUpperCase()?.slice(0, 4) || '?';
      const isRunning = Object.values(state.runningSessions).some(s => s.versionId === v.id);
      const dc = isRunning ? 'running' : v.launchable ? 'ok' : 'missing';
      const sel = state.selectedVersion?.id === v.id ? 'selected' : '';
      const rc = isRunning ? 'is-running' : '';
      const dim = v.launchable ? '' : 'dimmed';
      const pd = parseVersionDisplay(v.id, v, state.versions);
      const vLabel = pd.hint ? `${esc(pd.name)} <span class="version-hint">${esc(pd.hint)}</span>` : esc(pd.name);
      html += `<button type="button" class="version-item ${dim} ${sel} ${rc}" data-id="${v.id}" aria-pressed="${sel ? 'true' : 'false'}" aria-label="Select version ${esc(v.id)}" style="animation-delay:${i * 15}ms"><div class="version-dot ${dc}"></div><span class="version-name">${vLabel}</span>${isRunning ? '<span class="version-running-tag">LIVE</span>' : ''}<span class="version-badge ${bc}">${bt}</span></button>`;
    });
    html += `</div>`;
  };

  renderGroup('release', 'Releases', groups.release);
  renderGroup('modded', 'Modded', groups.modded);
  renderGroup('snapshot', 'Snapshots', groups.snapshot);
  renderGroup('other', 'Other', groups.other);
  dom.versionList.innerHTML = html;

  dom.versionList.querySelectorAll('.version-item').forEach(el => {
    const v = state.versions.find(version => version.id === el.dataset.id);
    el.addEventListener('focus', () => {
      if (!v || state.selectedVersion?.id === v.id) return;
      state.selectedVersion = v;
      setPage('launcher');
      renderSelectedVersion();
    });
    el.addEventListener('click', (e) => {
      if (e.button !== 0) return;
      if (v) selectVersion(v);
    });
    el.addEventListener('contextmenu', (e) => {
      if (v) {
        e.preventDefault();
        e.stopPropagation();
        state.selectedVersion = v;
        setPage('launcher');
        renderSelectedVersion();
        showContextMenu(e, v);
      }
    });
  });

  dom.versionList.querySelectorAll('.version-group-label').forEach(label => {
    label.addEventListener('click', () => {
      const key = label.dataset.group;
      local.collapsedGroups[key] = !local.collapsedGroups[key];
      saveLocalState();
      label.classList.toggle('collapsed');
      const items = dom.versionList.querySelector(`[data-group-items="${key}"]`);
      if (items) items.classList.toggle('collapsed');
    });
  });
}

// Exact copy of filterVersions from lines 1039-1046
export function filterVersions(versions) {
  let list = versions;
  if (state.filter === 'release') list = list.filter(v => v.type === 'release' && !v.inherits_from);
  else if (state.filter === 'snapshot') list = list.filter(v => v.type === 'snapshot' && !v.inherits_from);
  else if (state.filter === 'modded') list = list.filter(v => !!v.inherits_from);
  if (state.search) { const q = state.search.toLowerCase(); list = list.filter(v => v.id.toLowerCase().includes(q)); }
  return list;
}

// --- Version Watcher ---
// Exact copy of watchVersions from lines 798-820
export function watchVersions() {
  if (state.versionWatcher) state.versionWatcher.close();
  const es = new EventSource(`${API}/versions/watch`);
  state.versionWatcher = es;
  es.addEventListener('versions_changed', (e) => {
    try {
      const d = JSON.parse(e.data);
      const newVersions = d.versions || [];
      state.versions = newVersions;
      renderInstanceList();
      if (state.selectedInstance) {
        renderSelectedInstance();
      }
    } catch {}
  });
  es.onerror = () => {
    es.close();
    state.versionWatcher = null;
    setTimeout(watchVersions, 5000);
  };
}
