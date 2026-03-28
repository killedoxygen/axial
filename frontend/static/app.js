// ═══════════════════════════════════════════
// Croopor — Frontend Application
// ═══════════════════════════════════════════

const API = '/api/v1';
const STORAGE_KEY = 'croopor_ui';

// ── Local UI State (persisted to localStorage) ──

const defaults = {
  theme: 'obsidian',
  logExpanded: false,
  collapsedGroups: {},
  sidebarFilter: 'all',
};

function loadLocalState() {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw ? { ...defaults, ...JSON.parse(raw) } : { ...defaults };
  } catch { return { ...defaults }; }
}

function saveLocalState() {
  try { localStorage.setItem(STORAGE_KEY, JSON.stringify(local)); } catch {}
}

const local = loadLocalState();

// ── App State ──

const state = {
  versions: [],
  catalog: null,
  config: null,
  systemInfo: null,
  devMode: false,
  selectedVersion: null,
  activeSession: null,
  eventSource: null,
  installEventSource: null,
  logLines: 0,
  catalogFilter: 'release',
  search: '',
  catalogSearch: '',
  gameRunning: false,
  runningVersionId: null,
  installing: false,
  launching: false,
};

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => document.querySelectorAll(sel);

const dom = {
  versionList: $('#version-list'),
  versionSearch: $('#version-search'),
  emptyState: $('#empty-state'),
  emptyTitle: $('#empty-title'),
  emptySub: $('#empty-sub'),
  emptyAddBtn: $('#empty-add-btn'),
  versionDetail: $('#version-detail'),
  detailId: $('#detail-id'),
  detailBadge: $('#detail-badge'),
  detailMeta: $('#detail-meta'),
  launchArea: $('#launch-area'),
  launchBtn: $('#launch-btn'),
  launchingArea: $('#launching-area'),
  launchAscii: $('#launch-ascii'),
  runningArea: $('#running-area'),
  runningAscii: $('#running-ascii'),
  runningVersion: $('#running-version'),
  runningPid: $('#running-pid'),
  runningUptime: $('#running-uptime'),
  killBtn: $('#kill-btn'),
  notLaunchable: $('#not-launchable'),
  notLaunchableText: $('#not-launchable-text'),
  installArea: $('#install-area'),
  installText: $('#install-text'),
  installBtn: $('#install-btn'),
  installProgress: $('#install-progress'),
  progressFill: $('#progress-fill'),
  progressText: $('#progress-text'),
  usernameInput: $('#username-input'),
  memorySlider: $('#memory-slider'),
  memoryValue: $('#memory-value'),
  memoryRec: $('#memory-rec'),
  logPanel: $('#log-panel'),
  logToggle: $('#log-toggle'),
  logContent: $('#log-content'),
  logLines: $('#log-lines'),
  logCount: $('#log-count'),
  settingsBtn: $('#settings-btn'),
  settingsModal: $('#settings-modal'),
  settingsClose: $('#settings-close'),
  settingsCancel: $('#settings-cancel'),
  settingsSave: $('#settings-save'),
  settingJavaPath: $('#setting-java-path'),
  settingWidth: $('#setting-width'),
  settingHeight: $('#setting-height'),
  javaRuntimes: $('#java-runtimes'),
  themePicker: $('#theme-picker'),
  addVersionBtn: $('#add-version-btn'),
  catalogModal: $('#catalog-modal'),
  catalogClose: $('#catalog-close'),
  catalogSearch: $('#catalog-search'),
  catalogList: $('#catalog-list'),
  onboarding: $('#onboarding'),
  onboardingStep1: $('#onboarding-step-1'),
  onboardingStep2: $('#onboarding-step-2'),
  onboardingStep3: $('#onboarding-step-3'),
  onboardingUsername: $('#onboarding-username'),
  onboardingRamInfo: $('#onboarding-ram-info'),
  onboardingMemorySlider: $('#onboarding-memory-slider'),
  onboardingMemoryValue: $('#onboarding-memory-value'),
  onboardingRec: $('#onboarding-rec'),
  onboardingNext1: $('#onboarding-next-1'),
  onboardingNext2: $('#onboarding-next-2'),
  onboardingFinish: $('#onboarding-finish'),
  dot1: $('#dot-1'), dot2: $('#dot-2'), dot3: $('#dot-3'),
  devTools: $('#dev-tools'),
  devCleanup: $('#dev-cleanup'),
  devFlush: $('#dev-flush'),
};

// ── Theme ──

function applyTheme(theme) {
  document.documentElement.setAttribute('data-theme', theme);
  local.theme = theme;
  saveLocalState();
  // Update active swatch
  dom.themePicker?.querySelectorAll('.theme-swatch').forEach(s => {
    s.classList.toggle('active', s.dataset.theme === theme);
  });
}

// ── API ──

async function api(method, path, body) {
  const opts = { method, headers: { 'Content-Type': 'application/json' } };
  if (body) opts.body = JSON.stringify(body);
  const res = await fetch(`${API}${path}`, opts);
  return res.json();
}

// ── Memory ──

function getMemoryRecommendation(totalGB) {
  if (totalGB <= 4) return { rec: 2, text: 'Low RAM — 2 GB recommended' };
  if (totalGB <= 8) return { rec: 4, text: '4 GB recommended' };
  if (totalGB <= 16) return { rec: 6, text: '6 GB recommended' };
  return { rec: 8, text: '8 GB recommended' };
}

function updateMemoryRecText(val, totalGB) {
  if (!totalGB || !dom.memoryRec) return;
  if (val < 2) dom.memoryRec.textContent = '(low — may lag)';
  else if (val > totalGB * 0.75) dom.memoryRec.textContent = '(high — leave room for OS)';
  else dom.memoryRec.textContent = '';
}

// ── Init ──

async function init() {
  // Apply persisted local state
  applyTheme(local.theme);
  if (local.logExpanded) dom.logPanel.classList.add('expanded');

  // Set persisted sidebar filter
  state.filter = local.sidebarFilter;
  $$('.filter-chips .chip[data-filter]').forEach(c => {
    c.classList.toggle('active', c.dataset.filter === state.filter);
  });

  try {
    const [versionsRes, configRes, systemRes, statusRes] = await Promise.all([
      api('GET', '/versions'),
      api('GET', '/config'),
      api('GET', '/system').catch(() => null),
      api('GET', '/status').catch(() => null),
    ]);
    state.versions = versionsRes.versions || [];
    state.config = configRes;
    state.systemInfo = systemRes;
    state.devMode = statusRes?.dev_mode === true;
    if (state.devMode && dom.devTools) dom.devTools.classList.remove('hidden');

    applyConfig(state.config);
    applySystemInfo(state.systemInfo);
    renderVersionList();

    if (state.config && !state.config.onboarding_done) showOnboarding();
  } catch (err) {
    dom.versionList.innerHTML = `<div class="loading-placeholder">
      <span style="color:var(--red)">Failed to connect</span>
      <span style="color:var(--text-muted);font-size:10px">${err.message}</span></div>`;
  }
}

function applyConfig(cfg) {
  if (cfg.username) dom.usernameInput.value = cfg.username;
  if (cfg.max_memory_mb) {
    const gb = cfg.max_memory_mb / 1024;
    dom.memorySlider.value = gb;
    dom.memoryValue.textContent = fmtMem(gb);
  }
}

function applySystemInfo(info) {
  if (!info?.total_memory_mb) return;
  const totalGB = Math.floor(info.total_memory_mb / 1024);
  if (totalGB > 0) {
    dom.memorySlider.max = totalGB;
    const cur = parseFloat(dom.memorySlider.value);
    if (cur > totalGB) {
      dom.memorySlider.value = totalGB;
      dom.memoryValue.textContent = fmtMem(totalGB);
    }
    updateMemoryRecText(parseFloat(dom.memorySlider.value), totalGB);
  }
}

// ── Sidebar: Installed Versions Only ──

function renderVersionList() {
  const filtered = filterVersions(state.versions);

  if (state.versions.length === 0) {
    dom.versionList.innerHTML = `<div class="loading-placeholder">
      <span>No versions installed</span></div>`;
    dom.emptyTitle.textContent = 'No versions installed';
    dom.emptySub.textContent = 'Add a Minecraft version to get started';
    dom.emptyAddBtn.classList.remove('hidden');
    return;
  }

  dom.emptyAddBtn.classList.add('hidden');
  dom.emptyTitle.textContent = 'Select a version';
  dom.emptySub.textContent = 'Choose a Minecraft version from the sidebar to launch';

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
  const chevronSvg = `<svg class="version-group-chevron" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><polyline points="6 9 12 15 18 9"/></svg>`;

  const renderGroup = (key, label, versions) => {
    if (!versions.length) return;
    const collapsed = local.collapsedGroups[key];
    html += `<div class="version-group-label${collapsed ? ' collapsed' : ''}" data-group="${key}">${chevronSvg}${label} <span style="opacity:0.4;font-weight:400;margin-left:2px">${versions.length}</span></div>`;
    html += `<div class="version-group-items${collapsed ? ' collapsed' : ''}" data-group-items="${key}">`;
    versions.forEach((v, i) => {
      const isModded = !!v.inherits_from;
      const badgeClass = isModded ? 'badge-modded' : v.type === 'release' ? 'badge-release' : v.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
      const badgeText = isModded ? 'MOD' : v.type === 'release' ? 'REL' : v.type === 'snapshot' ? 'SNAP' : v.type?.toUpperCase()?.slice(0, 4) || '?';
      const isRunning = state.gameRunning && state.runningVersionId === v.id;
      const dotClass = isRunning ? 'running' : v.launchable ? 'ok' : 'missing';
      const dimClass = v.launchable ? '' : 'dimmed';
      const selected = state.selectedVersion?.id === v.id ? 'selected' : '';
      const runningClass = isRunning ? 'is-running' : '';
      html += `<div class="version-item ${dimClass} ${selected} ${runningClass}" data-id="${v.id}" style="animation-delay:${i*15}ms">
        <div class="version-dot ${dotClass}"></div>
        <span class="version-name">${esc(v.id)}</span>
        ${isRunning ? '<span class="version-running-tag">LIVE</span>' : ''}
        <span class="version-badge ${badgeClass}">${badgeText}</span></div>`;
    });
    html += `</div>`;
  };
  renderGroup('release', 'Releases', groups.release);
  renderGroup('modded', 'Modded', groups.modded);
  renderGroup('snapshot', 'Snapshots', groups.snapshot);
  renderGroup('other', 'Other', groups.other);
  dom.versionList.innerHTML = html;

  dom.versionList.querySelectorAll('.version-item').forEach(el => {
    el.addEventListener('click', () => {
      const v = state.versions.find(v => v.id === el.dataset.id);
      if (v) selectVersion(v);
    });
  });

  // Collapsible group headers
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

function filterVersions(versions) {
  let list = versions;
  if (state.filter === 'release') list = list.filter(v => v.type === 'release' && !v.inherits_from);
  else if (state.filter === 'snapshot') list = list.filter(v => v.type === 'snapshot' && !v.inherits_from);
  else if (state.filter === 'modded') list = list.filter(v => !!v.inherits_from);
  if (state.search) {
    const q = state.search.toLowerCase();
    list = list.filter(v => v.id.toLowerCase().includes(q));
  }
  return list;
}

// ── Version Selection ──

function selectVersion(version) {
  state.selectedVersion = version;
  dom.versionList.querySelectorAll('.version-item').forEach(el => el.classList.toggle('selected', el.dataset.id === version.id));

  dom.emptyState.classList.add('hidden');
  dom.versionDetail.classList.remove('hidden');
  dom.detailId.textContent = version.id;

  const isModded = !!version.inherits_from;
  const badgeClass = isModded ? 'badge-modded' : version.type === 'release' ? 'badge-release' : version.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
  dom.detailBadge.className = `detail-badge ${badgeClass}`;
  dom.detailBadge.textContent = isModded ? 'Modded' : version.type === 'release' ? 'Release' : version.type === 'snapshot' ? 'Snapshot' : version.type || 'Unknown';

  let meta = '';
  if (version.java_component) meta += `<span>${version.java_component}</span>`;
  if (version.java_major) meta += `<span>Java ${version.java_major}</span>`;
  if (version.inherits_from) meta += `<span>Based on ${esc(version.inherits_from)}</span>`;
  if (version.release_time) { const d = new Date(version.release_time); if (!isNaN(d)) meta += `<span>${d.toLocaleDateString()}</span>`; }
  dom.detailMeta.innerHTML = meta;

  // Hide all areas
  dom.launchArea.classList.add('hidden');
  dom.installArea.classList.add('hidden');
  dom.notLaunchable.classList.add('hidden');
  dom.launchingArea.classList.add('hidden');
  dom.runningArea.classList.add('hidden');
  resetInstallUI();

  if (state.launching && state.runningVersionId === version.id) {
    dom.launchingArea.classList.remove('hidden');
  } else if (state.gameRunning && state.runningVersionId === version.id) {
    dom.runningArea.classList.remove('hidden');
  } else if (version.launchable) {
    dom.launchArea.classList.remove('hidden');
  } else if (version.status === 'incomplete') {
    dom.installArea.classList.remove('hidden');
    dom.installText.textContent = version.status_detail || 'Game files need downloading';
    dom.installBtn.dataset.installTarget = version.needs_install || version.id;
  } else {
    dom.installArea.classList.remove('hidden');
    dom.installText.textContent = version.status_detail || 'Game files need downloading';
    dom.installBtn.dataset.installTarget = version.needs_install || version.id;
  }
}

// ── Install (for incomplete local versions) ──

async function installVersion() {
  if (!state.selectedVersion || state.installing) return;
  state.installing = true;

  const target = dom.installBtn.dataset.installTarget || state.selectedVersion.id;

  dom.installBtn.disabled = true;
  dom.installBtn.querySelector('.install-btn-text').textContent = 'INSTALLING...';
  dom.installProgress.classList.remove('hidden');
  dom.progressText.textContent = target !== state.selectedVersion.id
    ? `Installing base version ${target}...`
    : 'Starting download...';

  try {
    const res = await api('POST', '/install', { version_id: target });
    if (res.error) { showError(res.error); resetInstallUI(); return; }
    connectInstallSSE(res.install_id);
  } catch (err) {
    showError('Install failed: ' + err.message);
    resetInstallUI();
  }
}

async function installFromCatalog(versionId, manifestUrl) {
  if (state.installing) return;
  state.installing = true;

  try {
    const res = await api('POST', '/install', { version_id: versionId, manifest_url: manifestUrl });
    if (res.error) { showError(res.error); state.installing = false; return; }

    const btn = dom.catalogList.querySelector(`[data-install-id="${versionId}"]`);
    if (btn) { btn.disabled = true; btn.textContent = 'Installing...'; }

    connectInstallSSE(res.install_id, versionId);
  } catch (err) {
    showError('Install failed: ' + err.message);
    state.installing = false;
  }
}

function connectInstallSSE(installId, catalogVersionId) {
  if (state.installEventSource) state.installEventSource.close();

  const es = new EventSource(`${API}/install/${installId}/events`);
  state.installEventSource = es;

  es.addEventListener('progress', (e) => {
    const d = JSON.parse(e.data);
    let pct = 0;
    if (d.phase === 'version_json') pct = 5;
    else if (d.phase === 'client_jar') pct = 30;
    else if (d.phase === 'libraries' && d.total > 0) pct = 30 + Math.round((d.current / d.total) * 65);
    else if (d.phase === 'done') pct = 100;
    else if (d.phase === 'error') { showError(d.error); onInstallDone(catalogVersionId); return; }

    dom.progressFill.style.width = pct + '%';
    dom.progressText.textContent = d.phase === 'done' ? 'Complete!' :
      d.phase === 'libraries' ? `Libraries (${d.current}/${d.total})` :
      d.phase === 'client_jar' ? 'Downloading game...' :
      d.phase === 'version_json' ? 'Fetching version info...' : d.phase;

    if (d.done) onInstallDone(catalogVersionId);
  });

  es.onerror = () => { if (state.installing) onInstallDone(catalogVersionId); };
}

async function onInstallDone(catalogVersionId) {
  state.installing = false;
  if (state.installEventSource) { state.installEventSource.close(); state.installEventSource = null; }

  dom.progressFill.style.width = '100%';
  dom.progressText.textContent = 'Complete!';

  try {
    const res = await api('GET', '/versions');
    state.versions = res.versions || [];
    renderVersionList();

    if (catalogVersionId) {
      const btn = dom.catalogList.querySelector(`[data-install-id="${catalogVersionId}"]`);
      if (btn) { btn.outerHTML = `<span class="catalog-installed-badge">Installed</span>`; }
    }

    if (state.selectedVersion) {
      const updated = state.versions.find(v => v.id === state.selectedVersion.id);
      if (updated) selectVersion(updated);
    }
  } catch { resetInstallUI(); }
}

function resetInstallUI() {
  state.installing = false;
  if (dom.installBtn) {
    dom.installBtn.disabled = false;
    const txt = dom.installBtn.querySelector('.install-btn-text');
    if (txt) txt.textContent = 'INSTALL';
  }
  dom.installProgress.classList.add('hidden');
  dom.progressFill.style.width = '0%';
}

// ── Catalog Modal ──

async function openCatalog() {
  dom.catalogModal.classList.remove('hidden');
  dom.catalogSearch.value = '';
  state.catalogSearch = '';
  dom.catalogList.innerHTML = `<div class="loading-placeholder"><div class="spinner"></div><span>Loading available versions...</span></div>`;

  try {
    const res = await api('GET', '/catalog');
    state.catalog = res;
    renderCatalog();
  } catch (err) {
    dom.catalogList.innerHTML = `<div class="loading-placeholder"><span style="color:var(--red)">Failed to load catalog</span></div>`;
  }
}

function closeCatalog() { dom.catalogModal.classList.add('hidden'); }

function renderCatalog() {
  if (!state.catalog?.versions) return;

  let list = state.catalog.versions.filter(v => v.type === state.catalogFilter);
  if (state.catalogSearch) {
    const q = state.catalogSearch.toLowerCase();
    list = list.filter(v => v.id.toLowerCase().includes(q));
  }

  const display = list.slice(0, 50);
  const hasMore = list.length > 50;

  if (display.length === 0) {
    dom.catalogList.innerHTML = `<div class="loading-placeholder"><span>No versions found</span></div>`;
    return;
  }

  dom.catalogList.innerHTML = display.map(v => {
    const badgeClass = v.type === 'release' ? 'badge-release' : v.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
    const badgeText = v.type === 'release' ? 'REL' : v.type === 'snapshot' ? 'SNAP' : v.type.toUpperCase().slice(0, 4);
    const date = new Date(v.release_time);
    const dateStr = !isNaN(date) ? date.toLocaleDateString() : '';
    const action = v.installed
      ? `<span class="catalog-installed-badge">Installed</span>`
      : `<button class="catalog-install-btn" data-install-id="${esc(v.id)}" data-url="${esc(v.url)}">Install</button>`;
    return `<div class="catalog-item">
      <div class="catalog-item-info"><span class="catalog-item-id">${esc(v.id)}</span><span class="catalog-item-date">${dateStr}</span></div>
      <span class="version-badge ${badgeClass}">${badgeText}</span>
      ${action}</div>`;
  }).join('') + (hasMore ? `<div class="loading-placeholder"><span style="font-size:10px;color:var(--text-muted)">Showing 50 of ${list.length} — use search to narrow</span></div>` : '');

  dom.catalogList.querySelectorAll('.catalog-install-btn').forEach(btn => {
    btn.addEventListener('click', () => installFromCatalog(btn.dataset.installId, btn.dataset.url));
  });
}

// ── Launch ──

async function launchGame() {
  if (!state.selectedVersion || state.gameRunning || state.launching) return;
  const username = dom.usernameInput.value.trim() || 'Player';
  const maxMemMB = Math.round(parseFloat(dom.memorySlider.value) * 1024);

  state.launching = true;
  state.runningVersionId = state.selectedVersion.id;
  dom.launchArea.classList.add('hidden');
  dom.launchingArea.classList.remove('hidden');
  startLaunchSequence();
  renderVersionList();

  try {
    const res = await api('POST', '/launch', { version_id: state.selectedVersion.id, username, max_memory_mb: maxMemMB });
    if (res.error) { showError(res.error); endLaunchSequence(); resetLaunchBtn(); return; }

    state.activeSession = res.session_id;

    setTimeout(() => {
      state.launching = false;
      state.gameRunning = true;
      endLaunchSequence();
      dom.launchingArea.classList.add('hidden');
      dom.runningArea.classList.remove('hidden');
      dom.runningVersion.textContent = state.selectedVersion?.id || '';
      dom.runningPid.textContent = `PID ${res.pid}`;
      startRunningAnimation();
      startUptime();
      renderVersionList();
    }, 1800);

    dom.logPanel.classList.add('expanded');
    connectLaunchSSE(res.session_id);
    api('PUT', '/config', { username, max_memory_mb: maxMemMB, last_version_id: state.selectedVersion.id });
  } catch (err) {
    showError(err.message);
    endLaunchSequence();
    state.launching = false;
    state.runningVersionId = null;
    dom.launchingArea.classList.add('hidden');
    dom.launchArea.classList.remove('hidden');
    resetLaunchBtn();
    renderVersionList();
  }
}

function resetLaunchBtn() {
  dom.launchBtn.querySelector('.launch-btn-text').textContent = 'LAUNCH';
  dom.launchBtn.disabled = false;
}

function connectLaunchSSE(sessionId) {
  if (state.eventSource) state.eventSource.close();
  const es = new EventSource(`${API}/launch/${sessionId}/events`);
  state.eventSource = es;
  es.addEventListener('status', (e) => { const d = JSON.parse(e.data); if (d.state === 'exited') onGameExited(d.exit_code); });
  es.addEventListener('log', (e) => { const d = JSON.parse(e.data); appendLog(d.source, d.text); });
  es.onerror = () => { if (state.gameRunning) onGameExited(-1); };
}

function onGameExited(exitCode) {
  state.gameRunning = false;
  state.launching = false;
  state.runningVersionId = null;
  state.activeSession = null;
  if (state.eventSource) { state.eventSource.close(); state.eventSource = null; }
  stopRunningAnimation();
  stopUptime();
  dom.runningArea.classList.add('hidden');
  dom.launchingArea.classList.add('hidden');
  if (state.selectedVersion?.launchable) { dom.launchArea.classList.remove('hidden'); resetLaunchBtn(); }
  appendLog('system', `Game exited with code ${exitCode}`);
  renderVersionList();
}

async function killGame() {
  if (!state.activeSession) return;
  try { await api('POST', `/launch/${state.activeSession}/kill`); } catch (err) { showError('Failed to kill: ' + err.message); }
}

// ── Log Panel ──

function appendLog(source, text) {
  const line = document.createElement('div');
  line.className = `log-line ${source}`;
  line.textContent = text;
  dom.logLines.appendChild(line);
  state.logLines++;
  dom.logCount.textContent = `${state.logLines} lines`;
  dom.logContent.scrollTop = dom.logContent.scrollHeight;
}

// ── Settings ──

function openSettings() {
  dom.settingsModal.classList.remove('hidden');
  if (state.config) {
    dom.settingJavaPath.value = state.config.java_path_override || '';
    dom.settingWidth.value = state.config.window_width || '';
    dom.settingHeight.value = state.config.window_height || '';
  }
  // Mark active theme swatch
  dom.themePicker?.querySelectorAll('.theme-swatch').forEach(s => {
    s.classList.toggle('active', s.dataset.theme === local.theme);
  });
  loadJavaRuntimes();
}

function closeSettings() { dom.settingsModal.classList.add('hidden'); }

async function saveSettings() {
  const u = {};
  const jp = dom.settingJavaPath.value.trim();
  if (jp !== (state.config?.java_path_override || '')) u.java_path_override = jp;
  const w = parseInt(dom.settingWidth.value) || 0;
  const h = parseInt(dom.settingHeight.value) || 0;
  if (w > 0) u.window_width = w;
  if (h > 0) u.window_height = h;
  if (Object.keys(u).length) { const r = await api('PUT', '/config', u); if (!r.error) state.config = r; }
  closeSettings();
}

async function loadJavaRuntimes() {
  try {
    const res = await api('GET', '/java');
    const rt = res.runtimes || [];
    dom.javaRuntimes.innerHTML = rt.length === 0 ? '<span class="setting-hint">No runtimes detected</span>' :
      rt.map(r => `<div class="java-runtime-item"><span class="java-runtime-component">${esc(r.Component||r.component)}</span><span class="java-runtime-source">${esc(r.Source||r.source)}</span></div>`).join('');
  } catch { dom.javaRuntimes.innerHTML = '<span class="setting-hint">Failed to load</span>'; }
}

// ── Onboarding ──

function showOnboarding() {
  dom.onboarding.classList.remove('hidden');
  if (state.systemInfo?.total_memory_mb) {
    const gb = Math.floor(state.systemInfo.total_memory_mb / 1024);
    dom.onboardingRamInfo.textContent = `Your system has ${gb} GB of RAM`;
    dom.onboardingMemorySlider.max = gb;
    const { rec, text } = getMemoryRecommendation(gb);
    dom.onboardingMemorySlider.value = rec;
    dom.onboardingMemoryValue.textContent = fmtMem(rec);
    dom.onboardingRec.textContent = text;
  }
}

function onboardingStep(n) {
  [dom.onboardingStep1, dom.onboardingStep2, dom.onboardingStep3].forEach((s, i) => s.classList.toggle('hidden', i !== n - 1));
  [dom.dot1, dom.dot2, dom.dot3].forEach((d, i) => d.classList.toggle('active', i === n - 1));
}

async function finishOnboarding() {
  const username = dom.onboardingUsername.value.trim() || 'Player';
  const memGB = parseFloat(dom.onboardingMemorySlider.value);
  dom.usernameInput.value = username;
  dom.memorySlider.value = memGB;
  dom.memoryValue.textContent = fmtMem(memGB);
  try { const r = await api('PUT', '/config', { username, max_memory_mb: Math.round(memGB * 1024) }); if (!r.error) state.config = r; } catch {}
  try { await api('POST', '/onboarding/complete'); } catch {}
  dom.onboarding.classList.add('hidden');
}

// ── Launch Sequence Animation ──

const LAUNCH_FRAMES = [
  [
    '  ┌──────────┐  ',
    '  │ ▓▓▓▓▓▓▓▓ │  ',
    '  │ ▓▓    ▓▓ │  ',
    '  │ ▓▓    ▓▓ │  ',
    '  │ ▓▓▓▓▓▓▓▓ │  ',
    '  └──────────┘  ',
  ],
  [
    '  ┌──────────┐  ',
    '  │ ░▓▓▓▓▓▓░ │  ',
    '  │ ▓░░  ░░▓ │  ',
    '  │ ▓░░  ░░▓ │  ',
    '  │ ░▓▓▓▓▓▓░ │  ',
    '  └──────────┘  ',
  ],
  [
    '  ┌──────────┐  ',
    '  │ ░░▓▓▓▓░░ │  ',
    '  │ ░▓▓  ▓▓░ │  ',
    '  │ ░▓▓  ▓▓░ │  ',
    '  │ ░░▓▓▓▓░░ │  ',
    '  └──────────┘  ',
  ],
  [
    '  ╔══════════╗  ',
    '  ║ ▓▓▓▓▓▓▓▓ ║  ',
    '  ║ ▓▓ ◆◆ ▓▓ ║  ',
    '  ║ ▓▓ ◆◆ ▓▓ ║  ',
    '  ║ ▓▓▓▓▓▓▓▓ ║  ',
    '  ╚══════════╝  ',
  ],
  [
    '  ╔══════════╗  ',
    '  ║ ████████ ║  ',
    '  ║ ██ ◈◈ ██ ║  ',
    '  ║ ██ ◈◈ ██ ║  ',
    '  ║ ████████ ║  ',
    '  ╚══════════╝  ',
  ],
];

let launchSeqInterval = null;
let launchSeqFrame = 0;

function startLaunchSequence() {
  launchSeqFrame = 0;
  if (dom.launchAscii) dom.launchAscii.textContent = LAUNCH_FRAMES[0].join('\n');
  launchSeqInterval = setInterval(() => {
    launchSeqFrame = (launchSeqFrame + 1) % LAUNCH_FRAMES.length;
    if (dom.launchAscii) dom.launchAscii.textContent = LAUNCH_FRAMES[launchSeqFrame].join('\n');
  }, 350);
}

function endLaunchSequence() {
  if (launchSeqInterval) { clearInterval(launchSeqInterval); launchSeqInterval = null; }
}

// ── Running ASCII Animation ──

let runningAnimInterval = null;

const RUNNING_FRAMES = [
  ['╭─────╮', '│ ◈ ◈ │', '│  ▽  │', '╰─────╯'],
  ['╭─────╮', '│ ◇ ◇ │', '│  △  │', '╰─────╯'],
  ['╭─────╮', '│ ◆ ◆ │', '│  ▽  │', '╰─────╯'],
  ['╭─────╮', '│ ◇ ◇ │', '│  ○  │', '╰─────╯'],
];

function startRunningAnimation() {
  if (!dom.runningAscii) return;
  let frame = 0;
  dom.runningAscii.textContent = RUNNING_FRAMES[0].join('\n');
  runningAnimInterval = setInterval(() => {
    frame = (frame + 1) % RUNNING_FRAMES.length;
    dom.runningAscii.textContent = RUNNING_FRAMES[frame].join('\n');
  }, 800);
}

function stopRunningAnimation() {
  if (runningAnimInterval) { clearInterval(runningAnimInterval); runningAnimInterval = null; }
}

// ── Uptime Counter ──

let uptimeInterval = null;
let uptimeStart = 0;

function startUptime() {
  uptimeStart = Date.now();
  uptimeInterval = setInterval(() => {
    const elapsed = Math.floor((Date.now() - uptimeStart) / 1000);
    const m = Math.floor(elapsed / 60);
    const s = elapsed % 60;
    if (dom.runningUptime) dom.runningUptime.textContent = `${m}:${s.toString().padStart(2, '0')}`;
  }, 1000);
}

function stopUptime() {
  if (uptimeInterval) { clearInterval(uptimeInterval); uptimeInterval = null; }
}

// ── Utilities ──

function showError(msg) { appendLog('stderr', `ERROR: ${msg}`); dom.logPanel.classList.add('expanded'); }
function esc(s) { const d = document.createElement('div'); d.textContent = s; return d.innerHTML; }
function fmtMem(gb) { return gb === Math.floor(gb) ? `${gb} GB` : `${gb.toFixed(1)} GB`; }

// ── Event Bindings ──

dom.versionSearch.addEventListener('input', (e) => { state.search = e.target.value; renderVersionList(); });

$$('.filter-chips .chip[data-filter]').forEach(chip => {
  chip.addEventListener('click', () => {
    chip.parentElement.querySelectorAll('.chip').forEach(c => c.classList.remove('active'));
    chip.classList.add('active');
    state.filter = chip.dataset.filter;
    local.sidebarFilter = state.filter;
    saveLocalState();
    renderVersionList();
  });
});

dom.memorySlider.addEventListener('input', () => {
  const v = parseFloat(dom.memorySlider.value);
  dom.memoryValue.textContent = fmtMem(v);
  updateMemoryRecText(v, state.systemInfo?.total_memory_mb ? Math.floor(state.systemInfo.total_memory_mb / 1024) : null);
});

dom.usernameInput.addEventListener('blur', () => {
  const u = dom.usernameInput.value.trim();
  if (u && u !== state.config?.username) { api('PUT', '/config', { username: u }); if (state.config) state.config.username = u; }
});

dom.launchBtn.addEventListener('click', launchGame);
dom.installBtn.addEventListener('click', installVersion);
dom.killBtn.addEventListener('click', killGame);

dom.logToggle.addEventListener('click', () => {
  dom.logPanel.classList.toggle('expanded');
  local.logExpanded = dom.logPanel.classList.contains('expanded');
  saveLocalState();
});

// Settings
dom.settingsBtn.addEventListener('click', openSettings);
dom.settingsClose.addEventListener('click', closeSettings);
dom.settingsCancel.addEventListener('click', closeSettings);
dom.settingsSave.addEventListener('click', saveSettings);
dom.settingsModal.addEventListener('click', (e) => { if (e.target === dom.settingsModal) closeSettings(); });

// Theme picker
dom.themePicker?.querySelectorAll('.theme-swatch').forEach(swatch => {
  swatch.addEventListener('click', () => applyTheme(swatch.dataset.theme));
});

// Catalog
dom.addVersionBtn.addEventListener('click', openCatalog);
dom.emptyAddBtn.addEventListener('click', openCatalog);
dom.catalogClose.addEventListener('click', closeCatalog);
dom.catalogModal.addEventListener('click', (e) => { if (e.target === dom.catalogModal) closeCatalog(); });
dom.catalogSearch.addEventListener('input', (e) => { state.catalogSearch = e.target.value; renderCatalog(); });

$$('.chip[data-catalog-filter]').forEach(chip => {
  chip.addEventListener('click', () => {
    chip.parentElement.querySelectorAll('.chip').forEach(c => c.classList.remove('active'));
    chip.classList.add('active');
    state.catalogFilter = chip.dataset.catalogFilter;
    renderCatalog();
  });
});

// Onboarding
dom.onboardingNext1.addEventListener('click', () => onboardingStep(2));
dom.onboardingNext2.addEventListener('click', () => onboardingStep(3));
dom.onboardingFinish.addEventListener('click', finishOnboarding);
dom.onboardingMemorySlider.addEventListener('input', () => {
  const v = parseFloat(dom.onboardingMemorySlider.value);
  dom.onboardingMemoryValue.textContent = fmtMem(v);
  const gb = state.systemInfo?.total_memory_mb ? Math.floor(state.systemInfo.total_memory_mb / 1024) : null;
  if (gb) {
    if (v < 2) dom.onboardingRec.textContent = 'Low — may cause issues';
    else if (v > gb * 0.75) dom.onboardingRec.textContent = 'High — leave room for OS';
    else dom.onboardingRec.textContent = getMemoryRecommendation(gb).text;
  }
});

// Dev tools
if (dom.devCleanup) dom.devCleanup.addEventListener('click', async () => {
  if (!confirm('This will remove all installed versions.\nWorlds, mods, and resource packs will be backed up.\n\nContinue?')) return;
  dom.devCleanup.disabled = true;
  dom.devCleanup.textContent = 'Working...';
  try {
    const res = await api('POST', '/dev/cleanup-versions');
    if (res.error) { showError(res.error); } else {
      appendLog('system', `Backup saved to: ${res.backup_dir}`);
      appendLog('system', `Backed up: ${(res.backed_up||[]).join(', ')}`);
      appendLog('system', `Removed ${res.removed} versions`);
      const vr = await api('GET', '/versions');
      state.versions = vr.versions || [];
      state.selectedVersion = null;
      dom.versionDetail.classList.add('hidden');
      dom.emptyState.classList.remove('hidden');
      renderVersionList();
    }
  } catch (err) { showError(err.message); }
  dom.devCleanup.disabled = false;
  dom.devCleanup.textContent = 'Cleanup Versions';
});

if (dom.devFlush) dom.devFlush.addEventListener('click', async () => {
  if (!confirm('This will delete all Croopor settings and cached runtimes.\nThe app will restart from the onboarding screen.\n\nContinue?')) return;
  try {
    await api('POST', '/dev/flush');
    localStorage.removeItem(STORAGE_KEY);
    location.reload();
  } catch (err) { showError(err.message); }
});

document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    if (!dom.settingsModal.classList.contains('hidden')) closeSettings();
    else if (!dom.catalogModal.classList.contains('hidden')) closeCatalog();
  }
});

// ── Boot ──
init();
