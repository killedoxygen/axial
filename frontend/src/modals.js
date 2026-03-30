import { state, dom, local } from './state.js';
import { api } from './api.js';
import { Sound } from './sound.js';
import { Music } from './music.js';
import { esc, fmtMem, showError, getMemoryRecommendation } from './utils.js';
import { positionFieldMarker } from './theme.js';
import { renderInstanceList } from './sidebar.js';
import { selectInstance } from './instance.js';
import { installVersion } from './install.js';

export async function openNewInstanceFlow() {
  // Dismiss any existing instance modal
  document.getElementById('new-instance-modal')?.remove();

  // Load catalog if not cached
  if (!state.catalog) {
    try {
      state.catalog = await api('GET', '/catalog');
    } catch {
      showError('Failed to load version catalog');
      return;
    }
  }

  const allVersions = state.catalog.versions || [];
  let filter = 'release';
  let search = '';
  let selectedVersionId = null;
  let page = 0;
  const PAGE_SIZE = 50;

  function defaultName() {
    const base = 'Instance';
    const names = new Set(state.instances.map(i => i.name));
    if (!names.has(base)) return base;
    for (let n = 2; ; n++) {
      const alt = `${base} ${n}`;
      if (!names.has(alt)) return alt;
    }
  }

  const modal = document.createElement('div');
  modal.className = 'modal-overlay';
  modal.id = 'new-instance-modal';
  modal.innerHTML = `
    <div class="modal" style="width:480px">
      <div class="modal-header">
        <span class="modal-title">New Instance</span>
        <button class="icon-btn modal-close" id="new-instance-close">&times;</button>
      </div>
      <div style="padding:16px 18px;display:flex;flex-direction:column;gap:14px">
        <div>
          <label class="detail-prop-label" style="display:block;margin-bottom:6px;padding:0">Name</label>
          <input type="text" id="new-instance-name" class="field-input" placeholder="My Instance" spellcheck="false" autocomplete="off" style="width:100%;box-sizing:border-box">
          <div id="ni-name-error" style="font-size:11px;color:var(--red);margin-top:4px;display:none"></div>
        </div>
        <div>
          <label class="detail-prop-label" style="display:block;margin-bottom:6px;padding:0">Version</label>
          <input type="text" id="ni-version-search" class="field-input" placeholder="Search versions..." spellcheck="false" style="width:100%;box-sizing:border-box;margin-bottom:8px">
          <div class="filter-chips" id="ni-filters">
            <button class="chip active" data-nif="release">Release</button>
            <button class="chip" data-nif="snapshot">Snapshot</button>
            <button class="chip" data-nif="old_beta">Beta</button>
            <button class="chip" data-nif="old_alpha">Alpha</button>
          </div>
          <div class="ni-version-list" id="ni-version-list"></div>
        </div>
        <button class="btn-primary" id="new-instance-create" style="align-self:flex-end;margin-top:4px">Create</button>
      </div>
    </div>
  `;
  document.body.appendChild(modal);
  Sound.ui('bright');

  const nameInput = document.getElementById('new-instance-name');
  const nameError = document.getElementById('ni-name-error');
  const searchInput = document.getElementById('ni-version-search');
  const versionList = document.getElementById('ni-version-list');
  nameInput?.focus();

  function isAutoName(val) {
    return !val || /^Instance( \d+)?$/.test(val);
  }

  function validateName(name) {
    if (!name) return 'Name is required';
    if (state.instances.some(i => i.name === name)) return 'An instance with this name already exists';
    return null;
  }

  function showNameError(msg) {
    if (nameError) { nameError.textContent = msg; nameError.style.display = 'block'; }
  }
  function clearNameError() {
    if (nameError) nameError.style.display = 'none';
  }

  function renderVersionPicker() {
    let list = allVersions.filter(v => v.type === filter);
    if (search) { const q = search.toLowerCase(); list = list.filter(v => v.id.toLowerCase().includes(q)); }
    const total = list.length;
    const start = page * PAGE_SIZE;
    const display = list.slice(start, start + PAGE_SIZE);
    if (!display.length && total === 0) {
      versionList.innerHTML = '<div style="padding:12px;text-align:center;color:var(--text-muted);font-size:12px">No versions found</div>';
      return;
    }
    const totalPages = Math.ceil(total / PAGE_SIZE);
    let html = display.map(v => {
      const selected = v.id === selectedVersionId;
      return `<div class="ni-version-item${selected ? ' selected' : ''}" data-vid="${esc(v.id)}"><span class="ni-version-id">${esc(v.id)}</span>${v.installed ? '<span class="ni-installed-badge">Installed</span>' : ''}</div>`;
    }).join('');
    if (totalPages > 1) {
      html += `<div class="ni-pagination"><button class="ni-page-btn" id="ni-prev" ${page === 0 ? 'disabled' : ''}>&lsaquo;</button><span class="ni-page-info">${page + 1} / ${totalPages}</span><button class="ni-page-btn" id="ni-next" ${page >= totalPages - 1 ? 'disabled' : ''}>&rsaquo;</button></div>`;
    }
    versionList.innerHTML = html;
    versionList.querySelectorAll('.ni-version-item').forEach(el => {
      el.addEventListener('click', () => {
        selectedVersionId = el.dataset.vid;
        if (isAutoName(nameInput?.value.trim())) nameInput.value = defaultName();
        clearNameError();
        renderVersionPicker();
        Sound.ui('click');
      });
    });
    document.getElementById('ni-prev')?.addEventListener('click', () => { if (page > 0) { page--; renderVersionPicker(); } });
    document.getElementById('ni-next')?.addEventListener('click', () => { if (page < totalPages - 1) { page++; renderVersionPicker(); } });
  }

  // Select first version by default
  const firstList = allVersions.filter(v => v.type === filter);
  if (firstList.length > 0) {
    selectedVersionId = firstList[0].id;
    nameInput.value = defaultName();
  }
  renderVersionPicker();

  searchInput?.addEventListener('input', (e) => { search = e.target.value; page = 0; renderVersionPicker(); });
  document.getElementById('ni-filters')?.querySelectorAll('.chip').forEach(chip => {
    chip.addEventListener('click', () => {
      document.getElementById('ni-filters').querySelectorAll('.chip').forEach(c => c.classList.remove('active'));
      chip.classList.add('active');
      filter = chip.dataset.nif;
      page = 0;
      renderVersionPicker();
    });
  });

  nameInput?.addEventListener('input', () => clearNameError());

  const close = () => { modal.remove(); Sound.ui('soft'); };
  document.getElementById('new-instance-close')?.addEventListener('click', close);
  modal.addEventListener('click', (e) => { if (e.target === modal) close(); });

  document.getElementById('new-instance-create')?.addEventListener('click', async () => {
    const name = nameInput?.value.trim();
    const err = validateName(name);
    if (err) { showNameError(err); nameInput?.focus(); return; }
    if (!selectedVersionId) return;

    try {
      const res = await api('POST', '/instances', { name, version_id: selectedVersionId });
      if (res.error) { showError(res.error); return; }
      state.instances.push(res);
      const needsInstall = !allVersions.find(v => v.id === selectedVersionId)?.installed;
      close();
      renderInstanceList();
      selectInstance(res);
      Sound.ui('affirm');
      // Auto-install if version not yet installed
      if (needsInstall) installVersion(selectedVersionId);
    } catch (err) {
      showError(err.message);
    }
  });

  nameInput?.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') document.getElementById('new-instance-create')?.click();
  });
}

export function showSetup() {
  return new Promise(async (resolve) => {
    dom.setupOverlay?.classList.remove('hidden');

    // Load the default path for the "create new" option
    try {
      const defaults = await api('GET', '/setup/defaults');
      if (dom.setupNewPath) dom.setupNewPath.value = defaults.default_path || '';
    } catch {}

    function hideSetup() {
      dom.setupOverlay?.classList.add('hidden');
      resolve();
    }

    function showPathError(msg) {
      if (dom.setupPathError) {
        dom.setupPathError.textContent = msg;
        dom.setupPathError.classList.remove('hidden');
      }
    }
    function clearPathError() {
      if (dom.setupPathError) dom.setupPathError.classList.add('hidden');
    }

    // "Use this path" flow
    dom.setupUseBtn?.addEventListener('click', async () => {
      clearPathError();
      const path = dom.setupPathInput?.value.trim();
      if (!path) { showPathError('Please enter a path'); return; }
      dom.setupUseBtn.disabled = true;
      dom.setupUseBtn.textContent = 'Checking...';
      try {
        const res = await api('POST', '/setup/set-dir', { path });
        if (res.error) { showPathError(res.error); return; }
        hideSetup();
      } catch (err) {
        showPathError(err.message || 'Failed to set directory');
      } finally {
        dom.setupUseBtn.disabled = false;
        dom.setupUseBtn.textContent = 'Use this path';
      }
    });

    // "Browse" button
    dom.setupBrowseBtn?.addEventListener('click', async () => {
      dom.setupBrowseBtn.disabled = true;
      dom.setupBrowseBtn.textContent = 'Opening...';
      try {
        const res = await api('POST', '/setup/browse');
        if (res.path) {
          dom.setupPathInput.value = res.path;
          clearPathError();
        }
      } catch {}
      dom.setupBrowseBtn.disabled = false;
      dom.setupBrowseBtn.textContent = 'Browse';
    });

    // "Create & Continue" flow
    dom.setupInitBtn?.addEventListener('click', async () => {
      const path = dom.setupNewPath?.value.trim();
      if (!path) return;
      dom.setupInitBtn.disabled = true;
      dom.setupInitBtn.textContent = 'Creating...';
      try {
        const res = await api('POST', '/setup/init', { path });
        if (res.error) { showPathError(res.error); return; }
        hideSetup();
      } catch (err) {
        showPathError(err.message || 'Failed to create directory');
      } finally {
        dom.setupInitBtn.disabled = false;
        dom.setupInitBtn.textContent = 'Create & Continue';
      }
    });
  });
}

export function showOnboarding() {
  dom.onboarding?.classList.remove('hidden');
  if (state.systemInfo?.total_memory_mb) {
    const gb = Math.floor(state.systemInfo.total_memory_mb / 1024);
    if (dom.onboardingRamInfo) dom.onboardingRamInfo.textContent = `Your system has ${gb} GB of RAM`;
    if (dom.onboardingMemorySlider) {
      dom.onboardingMemorySlider.max = gb;
      const { rec, text } = getMemoryRecommendation(gb);
      dom.onboardingMemorySlider.value = rec;
      if (dom.onboardingMemoryValue) dom.onboardingMemoryValue.textContent = fmtMem(rec);
      if (dom.onboardingRec) dom.onboardingRec.textContent = text;
    }
  }
  positionFieldMarker(dom.obColorField, dom.obColorFieldMarker, local.customHue, local.customVibrancy);
}

export function onboardingStep(n) {
  [dom.onboardingStep1, dom.onboardingStep2, dom.onboardingStep3, dom.onboardingStep4, dom.onboardingStep5].forEach((s, i) => { if (s) s.classList.toggle('hidden', i !== n - 1); });
  [dom.dot1, dom.dot2, dom.dot3, dom.dot4, dom.dot5].forEach((d, i) => { if (d) d.classList.toggle('active', i === n - 1); });
}

export async function finishOnboarding() {
  const username = dom.onboardingUsername?.value.trim() || 'Player';
  const memGB = parseFloat(dom.onboardingMemorySlider?.value || 4);
  const musicEnabled = dom.obMusicYes?.classList.contains('active') ?? false;
  if (dom.usernameInput) dom.usernameInput.value = username;
  if (dom.memorySlider) {
    dom.memorySlider.value = memGB;
    if (dom.memoryValue) dom.memoryValue.textContent = fmtMem(memGB);
  }
  try {
    const r = await api('PUT', '/config', { username, max_memory_mb: Math.round(memGB * 1024), music_enabled: musicEnabled, music_volume: 5 });
    if (!r.error) state.config = r;
  } catch {}
  try { await api('POST', '/onboarding/complete'); } catch {}
  dom.onboarding?.classList.add('hidden');
  Music.applyConfig({ music_enabled: musicEnabled, music_volume: 5 });
  if (musicEnabled) Music.play();
}
