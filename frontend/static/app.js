// ═══════════════════════════════════════════
// Croopor — Frontend Application
// ═══════════════════════════════════════════

const API = '/api/v1';

const state = {
  versions: [],
  config: null,
  systemInfo: null,
  selectedVersion: null,
  activeSession: null,
  eventSource: null,
  installEventSource: null,
  logLines: 0,
  filter: 'all',
  search: '',
  gameRunning: false,
  installing: false,
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
  // Onboarding
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
  dot1: $('#dot-1'),
  dot2: $('#dot-2'),
  dot3: $('#dot-3'),
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

// ── Memory Recommendation ──

function getMemoryRecommendation(totalGB) {
  if (totalGB <= 4) return { rec: 2, text: 'Low RAM system — 2 GB recommended' };
  if (totalGB <= 8) return { rec: 4, text: '4 GB recommended for most versions' };
  if (totalGB <= 16) return { rec: 6, text: '6 GB recommended' };
  return { rec: 8, text: '8 GB recommended for best performance' };
}

function updateMemoryRecText(sliderValue, totalGB) {
  if (!totalGB) return;
  const { rec } = getMemoryRecommendation(totalGB);
  const el = dom.memoryRec;
  if (!el) return;
  if (sliderValue < 2) {
    el.textContent = '(low — may cause issues)';
  } else if (sliderValue > totalGB * 0.75) {
    el.textContent = '(high — leave room for OS)';
  } else {
    el.textContent = '';
  }
}

// ── Initialization ──

async function init() {
  try {
    const [versionsRes, configRes, systemRes] = await Promise.all([
      api('GET', '/versions'),
      api('GET', '/config'),
      api('GET', '/system').catch(() => null),
    ]);

    state.versions = versionsRes.versions || [];
    state.config = configRes;
    state.systemInfo = systemRes;

    applyConfig(state.config);
    applySystemInfo(state.systemInfo);
    renderVersionList();

    // Show onboarding if not completed
    if (state.config && state.config.onboarding_done === false) {
      showOnboarding();
    }
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

function applySystemInfo(info) {
  if (!info || !info.total_memory_mb) return;
  const totalGB = Math.floor(info.total_memory_mb / 1024);
  if (totalGB > 0) {
    dom.memorySlider.max = totalGB;
    // Clamp current value
    const current = parseFloat(dom.memorySlider.value);
    if (current > totalGB) {
      dom.memorySlider.value = totalGB;
      dom.memoryValue.textContent = formatMemory(totalGB);
    }
    updateMemoryRecText(parseFloat(dom.memorySlider.value), totalGB);
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
      const isRemote = v.status === 'not_installed';
      const badgeClass = isModded ? 'badge-modded' :
        v.type === 'release' ? 'badge-release' :
        v.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
      const badgeText = isModded ? 'MOD' :
        v.type === 'release' ? 'REL' :
        v.type === 'snapshot' ? 'SNAP' : v.type?.toUpperCase()?.slice(0, 4) || '?';

      let dotClass;
      if (isRemote) {
        dotClass = 'remote';
      } else if (v.launchable) {
        dotClass = 'ok';
      } else {
        dotClass = 'missing';
      }

      const dimClass = isRemote ? 'dimmed' : (v.launchable ? '' : 'dimmed');
      const selected = state.selectedVersion?.id === v.id ? 'selected' : '';
      const tooltip = !v.launchable && !isRemote && v.missing?.length
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

  // When searching, search everything (no filter restrictions on remote)
  const isSearching = !!state.search;

  if (!isSearching) {
    // Default view: hide remote snapshots unless Snapshot filter is active
    // Remote versions that are releases are shown in "All" and "Release"
    // Remote snapshots are only shown when filter is "snapshot"
    if (state.filter === 'all') {
      list = list.filter(v => {
        if (v.status === 'not_installed' && v.type !== 'release') return false;
        return true;
      });
    } else if (state.filter === 'release') {
      list = list.filter(v => v.type === 'release' && !v.inherits_from);
    } else if (state.filter === 'snapshot') {
      list = list.filter(v => v.type === 'snapshot' && !v.inherits_from);
    } else if (state.filter === 'modded') {
      list = list.filter(v => !!v.inherits_from);
    }
  } else {
    // Apply type filter even during search (except "all" which shows everything)
    if (state.filter !== 'all') {
      list = list.filter(v => {
        if (state.filter === 'modded') return !!v.inherits_from;
        if (state.filter === 'release') return v.type === 'release' && !v.inherits_from;
        if (state.filter === 'snapshot') return v.type === 'snapshot' && !v.inherits_from;
        return true;
      });
    }
  }

  // Filter by search text
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

  // Hide all action areas first
  dom.launchArea.classList.add('hidden');
  dom.installArea.classList.add('hidden');
  dom.notLaunchable.classList.add('hidden');
  dom.runningArea.classList.add('hidden');

  // Reset install UI
  dom.installBtn.disabled = false;
  dom.installBtn.querySelector('.install-btn-text').textContent = 'INSTALL';
  dom.installProgress.classList.add('hidden');
  dom.progressFill.style.width = '0%';

  // Determine which area to show
  if (state.gameRunning && state.activeSession) {
    // Game is running
    dom.runningArea.classList.remove('hidden');
  } else if (version.status === 'not_installed') {
    // Remote version — needs install
    dom.installArea.classList.remove('hidden');
    dom.installText.textContent = 'This version is not installed';
  } else if (version.status === 'incomplete') {
    // Incomplete install
    dom.installArea.classList.remove('hidden');
    dom.installText.textContent = version.status_detail || 'Installation incomplete';
  } else if (version.launchable) {
    // Ready to launch
    dom.launchArea.classList.remove('hidden');
  } else {
    // Installed but not launchable (missing files etc.)
    dom.notLaunchable.classList.remove('hidden');
    const missingText = version.missing?.length
      ? `Missing: ${version.missing.join(', ')}`
      : 'Cannot launch — missing files';
    dom.notLaunchableText.textContent = missingText;
  }
}

// ── Install ──

async function installVersion() {
  if (!state.selectedVersion || state.installing) return;

  const versionId = state.selectedVersion.id;
  state.installing = true;

  dom.installBtn.disabled = true;
  dom.installBtn.querySelector('.install-btn-text').textContent = 'INSTALLING...';
  dom.installProgress.classList.remove('hidden');
  dom.progressFill.style.width = '0%';
  dom.progressText.textContent = 'Starting...';

  try {
    const res = await api('POST', '/install', { version_id: versionId });

    if (res.error) {
      showError(res.error);
      resetInstallButton();
      return;
    }

    const installId = res.install_id || res.id;
    if (installId) {
      connectInstallSSE(installId);
    } else {
      // No SSE — installation was synchronous or no ID returned
      await onInstallComplete();
    }
  } catch (err) {
    showError('Install failed: ' + err.message);
    resetInstallButton();
  }
}

function connectInstallSSE(installId) {
  if (state.installEventSource) {
    state.installEventSource.close();
  }

  const es = new EventSource(`${API}/install/${installId}/events`);
  state.installEventSource = es;

  es.addEventListener('progress', (e) => {
    const data = JSON.parse(e.data);
    if (data.percent !== undefined) {
      dom.progressFill.style.width = `${data.percent}%`;
    }
    if (data.text) {
      dom.progressText.textContent = data.text;
    }
  });

  es.addEventListener('status', (e) => {
    const data = JSON.parse(e.data);
    if (data.state === 'complete' || data.state === 'done') {
      onInstallComplete();
    } else if (data.state === 'error' || data.state === 'failed') {
      showError(data.message || 'Installation failed');
      resetInstallButton();
    }
  });

  es.addEventListener('error', (e) => {
    // Check if this is a normal close (installation done)
    if (es.readyState === EventSource.CLOSED) {
      // Connection was closed — could be success or failure
      // If we haven't already handled completion, re-fetch versions
      if (state.installing) {
        onInstallComplete();
      }
    }
  });

  es.onerror = () => {
    // SSE connection lost
    if (state.installing) {
      onInstallComplete();
    }
  };
}

async function onInstallComplete() {
  state.installing = false;

  if (state.installEventSource) {
    state.installEventSource.close();
    state.installEventSource = null;
  }

  dom.progressFill.style.width = '100%';
  dom.progressText.textContent = 'Complete!';

  // Re-fetch versions to get updated status
  try {
    const versionsRes = await api('GET', '/versions');
    state.versions = versionsRes.versions || [];
    renderVersionList();

    // Re-select the version if still selected
    if (state.selectedVersion) {
      const updated = state.versions.find(v => v.id === state.selectedVersion.id);
      if (updated) {
        selectVersion(updated);
      }
    }
  } catch (err) {
    showError('Failed to refresh versions: ' + err.message);
    resetInstallButton();
  }
}

function resetInstallButton() {
  state.installing = false;
  dom.installBtn.disabled = false;
  dom.installBtn.querySelector('.install-btn-text').textContent = 'INSTALL';
  dom.installProgress.classList.add('hidden');
  dom.progressFill.style.width = '0%';
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

// ── Onboarding ──

function showOnboarding() {
  dom.onboarding.classList.remove('hidden');

  // Set up system info for memory step
  if (state.systemInfo && state.systemInfo.total_memory_mb) {
    const totalGB = Math.floor(state.systemInfo.total_memory_mb / 1024);
    const totalMB = state.systemInfo.total_memory_mb;
    dom.onboardingRamInfo.textContent = `Your system has ${totalGB} GB of RAM (${totalMB} MB)`;
    dom.onboardingMemorySlider.max = totalGB;

    const { rec, text } = getMemoryRecommendation(totalGB);
    dom.onboardingMemorySlider.value = rec;
    dom.onboardingMemoryValue.textContent = formatMemory(rec);
    dom.onboardingRec.textContent = text;
  }
}

function onboardingGoToStep(step) {
  dom.onboardingStep1.classList.add('hidden');
  dom.onboardingStep2.classList.add('hidden');
  dom.onboardingStep3.classList.add('hidden');

  dom.dot1.classList.remove('active');
  dom.dot2.classList.remove('active');
  dom.dot3.classList.remove('active');

  if (step === 1) {
    dom.onboardingStep1.classList.remove('hidden');
    dom.dot1.classList.add('active');
  } else if (step === 2) {
    dom.onboardingStep2.classList.remove('hidden');
    dom.dot2.classList.add('active');
  } else if (step === 3) {
    dom.onboardingStep3.classList.remove('hidden');
    dom.dot3.classList.add('active');
  }
}

async function finishOnboarding() {
  const username = dom.onboardingUsername.value.trim() || 'Player';
  const memGB = parseFloat(dom.onboardingMemorySlider.value);
  const maxMemMB = Math.round(memGB * 1024);

  // Apply to main UI
  dom.usernameInput.value = username;
  dom.memorySlider.value = memGB;
  dom.memoryValue.textContent = formatMemory(memGB);

  // Save config
  try {
    const res = await api('PUT', '/config', {
      username: username,
      max_memory_mb: maxMemMB,
    });
    if (!res.error) {
      state.config = res;
    }
  } catch {
    // Non-critical — continue anyway
  }

  // Mark onboarding complete
  try {
    await api('POST', '/onboarding/complete');
  } catch {
    // Non-critical
  }

  // Hide overlay
  dom.onboarding.classList.add('hidden');
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
  const totalGB = state.systemInfo?.total_memory_mb
    ? Math.floor(state.systemInfo.total_memory_mb / 1024)
    : null;
  updateMemoryRecText(val, totalGB);
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

// Install
dom.installBtn.addEventListener('click', installVersion);

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

// Onboarding
dom.onboardingNext1.addEventListener('click', () => onboardingGoToStep(2));
dom.onboardingNext2.addEventListener('click', () => onboardingGoToStep(3));
dom.onboardingFinish.addEventListener('click', finishOnboarding);

dom.onboardingMemorySlider.addEventListener('input', () => {
  const val = parseFloat(dom.onboardingMemorySlider.value);
  dom.onboardingMemoryValue.textContent = formatMemory(val);
  const totalGB = state.systemInfo?.total_memory_mb
    ? Math.floor(state.systemInfo.total_memory_mb / 1024)
    : null;
  if (totalGB) {
    const { text } = getMemoryRecommendation(totalGB);
    if (val < 2) {
      dom.onboardingRec.textContent = 'Low — may cause performance issues';
    } else if (val > totalGB * 0.75) {
      dom.onboardingRec.textContent = 'High — leave room for your OS';
    } else {
      dom.onboardingRec.textContent = text;
    }
  }
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
