import { state, dom } from './state.js';
import { Sound } from './sound.js';
import { esc, setPage, formatRelativeTime, parseVersionDisplay } from './utils.js';
import { scrambleText, startRunningAnimation, stopRunningAnimation, startUptime, stopUptime } from './effects.js';
import { api } from './api.js';

// --- Selection ---

export function selectInstance(inst, options = {}) {
  const { silent = false } = options;
  if (!silent) { Sound.init(); Sound.tick(); }
  state.selectedInstance = inst;
  state.selectedVersion = inst ? state.versions.find(v => v.id === inst.version_id) || null : null;
  setPage('launcher');
  renderSelectedInstance();
  dom.versionList?.querySelectorAll('.version-item').forEach(el => {
    const selected = el.dataset.id === inst?.id;
    el.classList.toggle('selected', selected);
    el.setAttribute('aria-pressed', selected ? 'true' : 'false');
  });
}

export function selectVersion(version, options = {}) {
  const { silent = false } = options;
  if (!silent) { Sound.init(); Sound.tick(); }
  state.selectedVersion = version;
  setPage('launcher');
  renderSelectedVersion();
  // Note: renderVersionList is called by sidebar.js when needed
}

// --- Version Detail (legacy) ---

export function renderSelectedVersion() {
  const version = state.selectedVersion;
  if (!version) {
    dom.versionDetail?.classList.add('hidden');
    dom.emptyState?.classList.remove('hidden');
    return;
  }

  dom.versionList?.querySelectorAll('.version-item').forEach(el => {
    const selected = el.dataset.id === version.id;
    el.classList.toggle('selected', selected);
    el.setAttribute('aria-pressed', selected ? 'true' : 'false');
  });
  dom.emptyState?.classList.add('hidden');
  dom.versionDetail?.classList.remove('hidden');

  const pd = parseVersionDisplay(version.id, version, state.versions);
  scrambleText(dom.detailId, pd.name, 300);

  const isModded = !!version.inherits_from;
  const badgeClass = isModded ? 'badge-modded' : version.type === 'release' ? 'badge-release' : version.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
  dom.detailBadge.className = `detail-badge ${badgeClass}`;
  dom.detailBadge.textContent = isModded ? 'MOD' : version.type === 'release' ? 'REL' : version.type === 'snapshot' ? 'SNAP' : version.type?.toUpperCase()?.slice(0, 4) || '?';

  if (dom.detailProps) dom.detailProps.innerHTML = buildVersionProps(version, pd);
  refreshSelectedVersionActionState();
}

function buildVersionProps(version, pd) {
  let props = '';
  if (version.java_component) props += prop('Runtime', version.java_component, true);
  if (version.java_major) props += prop('Java', `Java ${version.java_major}`);
  if (pd?.hint) props += prop('Loader', pd.hint);
  else if (version.inherits_from) props += prop('Base', version.inherits_from);
  if (version.release_time) {
    const d = new Date(version.release_time);
    if (!isNaN(d)) props += prop('Released', d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' }));
  }
  const lastLaunched = formatLastLaunched(version.id);
  props += prop('Last launched', lastLaunched.text, lastLaunched.accent);
  if (version.status) props += prop('Status', version.launchable ? 'Ready' : version.status_detail || 'Incomplete', version.launchable);
  return props;
}

// --- Instance Detail ---

export function renderSelectedInstance() {
  const inst = state.selectedInstance;
  if (!inst) {
    dom.versionDetail?.classList.add('hidden');
    dom.emptyState?.classList.remove('hidden');
    return;
  }
  dom.emptyState?.classList.add('hidden');
  dom.versionDetail?.classList.remove('hidden');

  scrambleText(dom.detailId, inst.name, 300);

  const version = state.versions.find(v => v.id === inst.version_id);
  const vType = inst.version_type || version?.type || '';
  const isModded = version?.inherits_from;
  const badgeClass = isModded ? 'badge-modded' : vType === 'release' ? 'badge-release' : vType === 'snapshot' ? 'badge-snapshot' : 'badge-old';
  dom.detailBadge.className = `detail-badge ${badgeClass}`;
  dom.detailBadge.textContent = isModded ? 'MOD' : vType === 'release' ? 'REL' : vType === 'snapshot' ? 'SNAP' : vType?.toUpperCase()?.slice(0, 4) || '?';

  if (dom.detailProps) dom.detailProps.innerHTML = buildInstanceMeta(inst, version);

  let linksEl = document.getElementById('instance-links');
  if (!linksEl) {
    linksEl = document.createElement('div');
    linksEl.id = 'instance-links';
    linksEl.className = 'instance-links';
    dom.detailProps?.parentNode?.insertBefore(linksEl, dom.detailProps.nextSibling);
  }
  const isVanilla = !version?.inherits_from;
  linksEl.innerHTML = `<a class="instance-link" data-sub="saves">Open saves</a><a class="instance-link${isVanilla ? ' disabled' : ''}" data-sub="mods"${isVanilla ? ' title="No mod loader installed"' : ''}>Open mods</a><a class="instance-link" data-sub="resourcepacks">Open resources</a><a class="instance-link" data-sub="">Open folder</a>`;
  linksEl.querySelectorAll('.instance-link').forEach(a => {
    if (a.classList.contains('disabled')) return;
    a.addEventListener('click', () => {
      const sub = a.dataset.sub;
      api('POST', `/instances/${encodeURIComponent(inst.id)}/open-folder${sub ? '?sub=' + sub : ''}`);
      Sound.ui('click');
    });
  });

  refreshSelectedInstanceActionState();
}

function jvmPresetLabel(preset) {
  if (preset === 'aikar') return "Aikar's Flags";
  if (preset === 'zgc') return 'ZGC';
  return null;
}

function buildInstanceMeta(inst, version) {
  const parts = [];
  const pd = parseVersionDisplay(inst.version_id, version, state.versions);
  parts.push(pd.hint ? `${esc(pd.name)} <span class="meta-hint">${esc(pd.hint)}</span>` : esc(pd.name));
  if (version?.java_major) parts.push(`Java ${version.java_major}`);
  const preset = inst.jvm_preset || state.config?.jvm_preset || '';
  const presetText = jvmPresetLabel(preset);
  if (presetText) {
    const blocked = preset === 'zgc' && version?.java_major && version.java_major < 17;
    parts.push(blocked ? `<span style="opacity:.5" title="ZGC requires Java 17+">${presetText}</span>` : presetText);
  }
  if (version) {
    parts.push(version.launchable ? 'Ready' : version.status_detail || 'Incomplete');
  } else {
    parts.push('Version not installed');
  }
  if (inst.last_played_at) {
    const d = new Date(inst.last_played_at);
    if (!isNaN(d)) parts.push('Played ' + formatRelativeTime(d));
  } else {
    parts.push('Never played');
  }
  return `<div class="instance-meta">${parts.join(' <span class="meta-dot">·</span> ')}</div>`;
}

// --- Action State ---

function installTargetFor(inst) {
  if (!inst) return null;
  const v = state.versions.find(v => v.id === inst.version_id);
  return v?.needs_install || v?.id || inst.version_id;
}

function showActiveInstall() {
  show(dom.installArea);
  if (dom.installBtn) dom.installBtn.disabled = true;
  const label = dom.installBtn?.querySelector('.install-btn-text');
  if (label) label.textContent = 'INSTALLING...';
  show(dom.installProgress);
  if (dom.progressFill) dom.progressFill.style.width = (state.activeInstall?.pct || 0) + '%';
  if (dom.progressText) dom.progressText.textContent = state.activeInstall?.label || '';
}

function showQueuedInstall() {
  show(dom.installArea);
  if (dom.installBtn) dom.installBtn.disabled = true;
  const label = dom.installBtn?.querySelector('.install-btn-text');
  if (label) label.textContent = 'QUEUED';
}

export function refreshSelectedInstanceActionState() {
  const inst = state.selectedInstance;
  if (!inst) return;
  hideAllActions();

  // This instance is currently launching (brief animation state)
  if (state.launchingInstanceId === inst.id) {
    show(dom.launchingArea);
    return;
  }
  // Another instance is launching — block launch but allow viewing
  if (state.launchingInstanceId) {
    showNotLaunchable('Another launch is being prepared.');
    return;
  }

  // This instance is running
  const session = state.runningSessions[inst.id];
  if (session) {
    show(dom.runningArea);
    if (dom.runningVersion) dom.runningVersion.textContent = `${inst.name} (${inst.version_id})`;
    if (dom.runningPid) dom.runningPid.textContent = `PID ${session.pid}`;
    startRunningAnimation();
    startUptime(session.launchedAt);
    return;
  }

  const target = installTargetFor(inst);
  if (state.activeInstall?.versionId === target) { showActiveInstall(); return; }
  if (state.installQueue.some(q => q.versionId === target)) { showQueuedInstall(); return; }

  const version = state.versions.find(v => v.id === inst.version_id);
  if (!version) {
    show(dom.installArea);
    if (dom.installText) dom.installText.textContent = `Version ${inst.version_id} is not installed`;
    if (dom.installBtn) dom.installBtn.dataset.installTarget = inst.version_id;
    return;
  }

  if (version.launchable) {
    show(dom.launchArea);
  } else {
    show(dom.installArea);
    if (dom.installText) dom.installText.textContent = version.status_detail || 'Game files need downloading';
    if (dom.installBtn) dom.installBtn.dataset.installTarget = version.needs_install || version.id;
  }
}

export function refreshSelectedVersionActionState() {
  if (!state.selectedVersion) return;
  hideAllActions();
  const version = state.selectedVersion;

  if (state.launchingInstanceId) {
    showNotLaunchable('A launch is being prepared.');
    return;
  }

  const target = version.needs_install || version.id;
  if (state.activeInstall?.versionId === target) { showActiveInstall(); return; }
  if (state.installQueue.some(q => q.versionId === target)) { showQueuedInstall(); return; }

  if (version.launchable) {
    show(dom.launchArea);
  } else {
    show(dom.installArea);
    if (dom.installText) dom.installText.textContent = version.status_detail || 'Game files need downloading';
    if (dom.installBtn) dom.installBtn.dataset.installTarget = version.needs_install || version.id;
  }
}

// --- Install UI Reset (moved here to break circular dep with install.js) ---

export function resetInstallUI() {
  if (dom.installBtn) {
    dom.installBtn.disabled = false;
    const t = dom.installBtn.querySelector('.install-btn-text');
    if (t) t.textContent = 'INSTALL';
  }
  dom.installProgress?.classList.add('action-hidden');
  if (dom.progressFill) dom.progressFill.style.width = '0%';
}

// --- Helpers ---

function formatLastLaunched(versionId) {
  return { text: 'N/A', accent: false };
}

function prop(label, value, accent) {
  return `<div class="detail-prop"><span class="detail-prop-label">${label}</span><span class="detail-prop-value${accent ? ' accent' : ''}">${esc(String(value))}</span></div>`;
}

function hideAllActions() {
  [dom.launchArea, dom.launchingArea, dom.runningArea, dom.notLaunchable, dom.installArea].forEach(el => { if (el) el.classList.add('action-hidden'); });
  resetInstallUI();
  stopRunningAnimation();
  stopUptime();
}

function show(el) { if (el) el.classList.remove('action-hidden'); }

function showNotLaunchable(message) {
  if (dom.notLaunchableText) dom.notLaunchableText.textContent = message;
  show(dom.notLaunchable);
}
