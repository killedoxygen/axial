// ═══════════════════════════════════════════
// Croopor — Frontend Application
// ═══════════════════════════════════════════

const API = '/api/v1';

const state = {
  versions: [],
  config: null,
  selectedVersion: null,
  activeSession: null,
  eventSource: null,
  logLines: 0,
  filter: 'all',
  search: '',
  gameRunning: false,
};

// ── DOM References ──

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => document.querySelectorAll(sel);

const dom = {
  versionList: $('#version-list'),
  versionSearch: $('#version-search'),
  launchPanel: $('#launch-panel'),
  emptyState: $('#empty-state'),
  versionDetail: $('#version-detail'),
  detailId: $('#detail-id'),
  detailBadge: $('#detail-badge'),
  detailMeta: $('#detail-meta'),
  launchArea: $('#launch-area'),
  launchBtn: $('#launch-btn'),
  runningArea: $('#running-area'),
  runningPid: $('#running-pid'),
  killBtn: $('#kill-btn'),
  notLaunchable: $('#not-launchable'),
  notLaunchableText: $('#not-launchable-text'),
  usernameInput: $('#username-input'),
  memorySlider: $('#memory-slider'),
  memoryValue: $('#memory-value'),
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
};

// ── API Helpers ──

async function api(method, path, body) {
  const opts = {
    method,
    headers: { 'Content-Type': 'application/json' },
  };
  if (body) opts.body = JSON.stringify(body);
  const res = await fetch(`${API}${path}`, opts);
  return res.json();
}

// ── Initialization ──

async function init() {
  try {
    const [versionsRes, configRes] = await Promise.all([
      api('GET', '/versions'),
      api('GET', '/config'),
    ]);

    state.versions = versionsRes.versions || [];
    state.config = configRes;

    applyConfig(state.config);
    renderVersionList();
  } catch (err) {
    dom.versionList.innerHTML = `
      <div class="loading-placeholder">
        <span style="color: var(--red)">Failed to connect to backend</span>
        <span style="color: var(--text-muted); font-size: 11px">${err.message}</span>
      </div>`;
  }
}

function applyConfig(cfg) {
  if (cfg.username) dom.usernameInput.value = cfg.username;
  if (cfg.max_memory_mb) {
    const gb = cfg.max_memory_mb / 1024;
    dom.memorySlider.value = gb;
    dom.memoryValue.textContent = formatMemory(gb);
  }
}

// ── Version List Rendering ──

function renderVersionList() {
  const filtered = filterVersions(state.versions);

  if (filtered.length === 0) {
    dom.versionList.innerHTML = `
      <div class="loading-placeholder">
        <span>No versions found</span>
      </div>`;
    return;
  }

  // Group versions
  const groups = {
    release: [],
    snapshot: [],
    modded: [],
    other: [],
  };

  for (const v of filtered) {
    if (v.inherits_from) {
      groups.modded.push(v);
    } else if (v.type === 'release') {
      groups.release.push(v);
    } else if (v.type === 'snapshot') {
      groups.snapshot.push(v);
    } else {
      groups.other.push(v);
    }
  }

  let html = '';
  const renderGroup = (label, versions) => {
    if (versions.length === 0) return;
    html += `<div class="version-group-label">${label}</div>`;
    versions.forEach((v, i) => {
      const isModded = !!v.inherits_from;
      const badgeClass = isModded ? 'badge-modded' :
        v.type === 'release' ? 'badge-release' :
        v.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
      const badgeText = isModded ? 'MOD' :
        v.type === 'release' ? 'REL' :
        v.type === 'snapshot' ? 'SNAP' : v.type?.toUpperCase()?.slice(0, 4) || '?';
      const dotClass = v.launchable ? 'ok' : 'missing';
      const dimClass = v.launchable ? '' : 'dimmed';
      const selected = state.selectedVersion?.id === v.id ? 'selected' : '';
      const tooltip = !v.launchable && v.missing?.length
        ? `data-tooltip="Missing: ${v.missing.join(', ')}"`
        : '';
      const delay = `style="animation-delay: ${i * 20}ms"`;

      html += `<div class="version-item ${dimClass} ${selected}" data-id="${v.id}" ${tooltip} ${delay}>
        <div class="version-dot ${dotClass}"></div>
        <span class="version-name">${escapeHtml(v.id)}</span>
        <span class="version-badge ${badgeClass}">${badgeText}</span>
      </div>`;
    });
  };

  renderGroup('Releases', groups.release);
  renderGroup('Modded', groups.modded);
  renderGroup('Snapshots', groups.snapshot);
  renderGroup('Other', groups.other);

  dom.versionList.innerHTML = html;

  // Attach click handlers
  dom.versionList.querySelectorAll('.version-item').forEach(el => {
    el.addEventListener('click', () => {
      const id = el.dataset.id;
      const version = state.versions.find(v => v.id === id);
      if (version) selectVersion(version);
    });
  });
}

function filterVersions(versions) {
  let list = versions;

  // Filter by type
  if (state.filter !== 'all') {
    list = list.filter(v => {
      if (state.filter === 'modded') return !!v.inherits_from;
      if (state.filter === 'release') return v.type === 'release' && !v.inherits_from;
      if (state.filter === 'snapshot') return v.type === 'snapshot' && !v.inherits_from;
      return true;
    });
  }

  // Filter by search
  if (state.search) {
    const q = state.search.toLowerCase();
    list = list.filter(v => v.id.toLowerCase().includes(q));
  }

  return list;
}

// ── Version Selection ──

function selectVersion(version) {
  state.selectedVersion = version;

  // Update sidebar selection
  dom.versionList.querySelectorAll('.version-item').forEach(el => {
    el.classList.toggle('selected', el.dataset.id === version.id);
  });

  // Show detail panel
  dom.emptyState.classList.add('hidden');
  dom.versionDetail.classList.remove('hidden');

  dom.detailId.textContent = version.id;

  const isModded = !!version.inherits_from;
  const badgeClass = isModded ? 'badge-modded' :
    version.type === 'release' ? 'badge-release' :
    version.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
  const badgeText = isModded ? 'Modded' :
    version.type === 'release' ? 'Release' :
    version.type === 'snapshot' ? 'Snapshot' : version.type || 'Unknown';

  dom.detailBadge.className = `detail-badge ${badgeClass}`;
  dom.detailBadge.textContent = badgeText;

  // Meta info
  let meta = '';
  if (version.java_component) {
    meta += `<span>${version.java_component}</span>`;
  }
  if (version.java_major) {
    meta += `<span>Java ${version.java_major}</span>`;
  }
  if (version.inherits_from) {
    meta += `<span>Based on ${escapeHtml(version.inherits_from)}</span>`;
  }
  if (version.release_time) {
    const date = new Date(version.release_time);
    if (!isNaN(date)) {
      meta += `<span>${date.toLocaleDateString()}</span>`;
    }
  }
  dom.detailMeta.innerHTML = meta;

  // Show/hide launch or not-launchable
  if (version.launchable) {
    dom.launchArea.classList.remove('hidden');
    dom.notLaunchable.classList.add('hidden');
  } else {
    dom.launchArea.classList.add('hidden');
    dom.notLaunchable.classList.remove('hidden');
    const missingText = version.missing?.length
      ? `Missing: ${version.missing.join(', ')}`
      : 'Cannot launch — missing files';
    dom.notLaunchableText.textContent = missingText;
  }

  // Handle running state
  if (state.gameRunning && state.activeSession) {
    dom.launchArea.classList.add('hidden');
    dom.runningArea.classList.remove('hidden');
  } else {
    dom.runningArea.classList.add('hidden');
  }
}

// ── Launch ──

async function launchGame() {
  if (!state.selectedVersion || state.gameRunning) return;

  const username = dom.usernameInput.value.trim() || 'Player';
  const maxMemMB = Math.round(parseFloat(dom.memorySlider.value) * 1024);

  dom.launchBtn.querySelector('.launch-btn-text').textContent = 'LAUNCHING...';
  dom.launchBtn.disabled = true;

  try {
    const res = await api('POST', '/launch', {
      version_id: state.selectedVersion.id,
      username: username,
      max_memory_mb: maxMemMB,
    });

    if (res.error) {
      showError(res.error);
      resetLaunchButton();
      return;
    }

    state.activeSession = res.session_id;
    state.gameRunning = true;

    // Update UI to running state
    dom.launchArea.classList.add('hidden');
    dom.runningArea.classList.remove('hidden');
    dom.runningPid.textContent = `PID ${res.pid}`;

    // Open log panel
    dom.logPanel.classList.add('expanded');

    // Connect SSE
    connectSSE(res.session_id);

    // Save config
    api('PUT', '/config', {
      username: username,
      max_memory_mb: maxMemMB,
      last_version_id: state.selectedVersion.id,
    });

  } catch (err) {
    showError(err.message);
    resetLaunchButton();
  }
}

function resetLaunchButton() {
  dom.launchBtn.querySelector('.launch-btn-text').textContent = 'LAUNCH';
  dom.launchBtn.disabled = false;
}

// ── SSE ──

function connectSSE(sessionId) {
  if (state.eventSource) {
    state.eventSource.close();
  }

  const es = new EventSource(`${API}/launch/${sessionId}/events`);
  state.eventSource = es;

  es.addEventListener('status', (e) => {
    const data = JSON.parse(e.data);
    if (data.state === 'exited') {
      onGameExited(data.exit_code);
    }
  });

  es.addEventListener('log', (e) => {
    const data = JSON.parse(e.data);
    appendLog(data.source, data.text);
  });

  es.onerror = () => {
    // SSE connection lost — game might have exited
    if (state.gameRunning) {
      onGameExited(-1);
    }
  };
}

function onGameExited(exitCode) {
  state.gameRunning = false;
  state.activeSession = null;

  if (state.eventSource) {
    state.eventSource.close();
    state.eventSource = null;
  }

  dom.runningArea.classList.add('hidden');

  if (state.selectedVersion?.launchable) {
    dom.launchArea.classList.remove('hidden');
    resetLaunchButton();
  }

  appendLog('system', `Game exited with code ${exitCode}`);
}

// ── Kill ──

async function killGame() {
  if (!state.activeSession) return;
  try {
    await api('POST', `/launch/${state.activeSession}/kill`);
  } catch (err) {
    showError('Failed to kill: ' + err.message);
  }
}

// ── Log Panel ──

function appendLog(source, text) {
  const line = document.createElement('div');
  line.className = `log-line ${source}`;
  line.textContent = text;
  dom.logLines.appendChild(line);

  state.logLines++;
  dom.logCount.textContent = `${state.logLines} lines`;

  // Auto-scroll
  dom.logContent.scrollTop = dom.logContent.scrollHeight;
}

function toggleLog() {
  dom.logPanel.classList.toggle('expanded');
}

// ── Settings ──

function openSettings() {
  dom.settingsModal.classList.remove('hidden');

  // Populate fields
  if (state.config) {
    dom.settingJavaPath.value = state.config.java_path_override || '';
    dom.settingWidth.value = state.config.window_width || '';
    dom.settingHeight.value = state.config.window_height || '';
  }

  // Load Java runtimes
  loadJavaRuntimes();
}

function closeSettings() {
  dom.settingsModal.classList.add('hidden');
}

async function saveSettings() {
  const updates = {};

  const javaPath = dom.settingJavaPath.value.trim();
  if (javaPath !== (state.config?.java_path_override || '')) {
    updates.java_path_override = javaPath;
  }

  const width = parseInt(dom.settingWidth.value) || 0;
  const height = parseInt(dom.settingHeight.value) || 0;
  if (width > 0) updates.window_width = width;
  if (height > 0) updates.window_height = height;

  if (Object.keys(updates).length > 0) {
    const res = await api('PUT', '/config', updates);
    if (!res.error) {
      state.config = res;
    }
  }

  closeSettings();
}

async function loadJavaRuntimes() {
  try {
    const res = await api('GET', '/java');
    const runtimes = res.runtimes || [];

    if (runtimes.length === 0) {
      dom.javaRuntimes.innerHTML = '<span class="setting-hint">No Java runtimes detected</span>';
      return;
    }

    dom.javaRuntimes.innerHTML = runtimes.map(r => `
      <div class="java-runtime-item">
        <span class="java-runtime-component">${escapeHtml(r.Component || r.component)}</span>
        <span class="java-runtime-source">${escapeHtml(r.Source || r.source)}</span>
      </div>
    `).join('');
  } catch {
    dom.javaRuntimes.innerHTML = '<span class="setting-hint">Failed to load</span>';
  }
}

// ── Error Display ──

function showError(msg) {
  appendLog('stderr', `ERROR: ${msg}`);
  dom.logPanel.classList.add('expanded');
}

// ── Utilities ──

function escapeHtml(str) {
  const div = document.createElement('div');
  div.textContent = str;
  return div.innerHTML;
}

function formatMemory(gb) {
  if (gb === Math.floor(gb)) return `${gb} GB`;
  return `${gb.toFixed(1)} GB`;
}

// ── Event Bindings ──

// Search and filter
dom.versionSearch.addEventListener('input', (e) => {
  state.search = e.target.value;
  renderVersionList();
});

$$('.chip').forEach(chip => {
  chip.addEventListener('click', () => {
    $$('.chip').forEach(c => c.classList.remove('active'));
    chip.classList.add('active');
    state.filter = chip.dataset.filter;
    renderVersionList();
  });
});

// Memory slider
dom.memorySlider.addEventListener('input', () => {
  const val = parseFloat(dom.memorySlider.value);
  dom.memoryValue.textContent = formatMemory(val);
});

// Username save on blur
dom.usernameInput.addEventListener('blur', () => {
  const username = dom.usernameInput.value.trim();
  if (username && username !== state.config?.username) {
    api('PUT', '/config', { username });
    if (state.config) state.config.username = username;
  }
});

// Launch
dom.launchBtn.addEventListener('click', launchGame);

// Kill
dom.killBtn.addEventListener('click', killGame);

// Log toggle
dom.logToggle.addEventListener('click', toggleLog);

// Settings
dom.settingsBtn.addEventListener('click', openSettings);
dom.settingsClose.addEventListener('click', closeSettings);
dom.settingsCancel.addEventListener('click', closeSettings);
dom.settingsSave.addEventListener('click', saveSettings);

dom.settingsModal.addEventListener('click', (e) => {
  if (e.target === dom.settingsModal) closeSettings();
});

// Keyboard shortcut: Escape closes modal
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    if (!dom.settingsModal.classList.contains('hidden')) {
      closeSettings();
    }
  }
});

// ── Boot ──
init();
