import { state, dom, local, $, $$, cacheDom, STORAGE_KEY, saveLocalState, PRESET_HUES } from './state.js';
import { api } from './api.js';
import { Sound, bindButtonSounds, playSliderSound } from './sound.js';
import { Music } from './music.js';
import { applyTheme, initColorField, positionFieldMarker, findFixedLightness } from './theme.js';
import { Shortcuts, syncShortcutHints, renderShortcutEditor, startRecording, handleRecordKey } from './shortcuts.js';
import { fmtMem, showError, appendLog, setLogFilter, setPage, toggleShortcutHints, getMemoryRecommendation, updateMemoryRecText } from './utils.js';
import { renderInstanceList, watchVersions } from './sidebar.js';
import { selectInstance, renderSelectedInstance, refreshSelectedInstanceActionState } from './instance.js';
import { installVersion } from './install.js';
import { launchGame, killGame } from './launch.js';
import { openSettings, closeSettings, saveSettings, syncSettingsSectionNav } from './settings.js';
import { openNewInstanceFlow, showSetup, showOnboarding, onboardingStep, onboardingNext, onboardingBack, getObStep, finishOnboarding } from './modals.js';
import { hideContextMenu, bindContextMenu } from './context-menu.js';
import { closeDeleteWizard, bindDeleteWizard } from './delete-wizard.js';
import { showConfirm } from './dialogs.js';

async function init() {
  Shortcuts.load(local.shortcuts);
  cacheDom();
  applyTheme(local.theme, local.customHue, { silent: true, vibrancy: local.customVibrancy, lightness: local.lightness });
  Sound.enabled = local.sounds;
  Sound.warmup();
  if (dom.soundsToggle) dom.soundsToggle.checked = local.sounds;
  if (local.logExpanded) dom.logPanel?.classList.add('expanded');
  if (local.logHeight && dom.logPanel) dom.logPanel.style.setProperty('--log-h', `${local.logHeight}px`);
  state.filter = local.sidebarFilter;
  $$('.filter-chips .chip[data-filter]').forEach(c => c.classList.toggle('active', c.dataset.filter === state.filter));
  positionFieldMarker(dom.colorField, dom.colorFieldMarker, local.customHue, local.customVibrancy);
  syncShortcutHints();
  renderShortcutEditor();
  setPage('launcher');

  try {
    const [configRes, systemRes, statusRes] = await Promise.all([
      api('GET', '/config'),
      api('GET', '/system').catch(() => null),
      api('GET', '/status').catch(() => null),
    ]);
    state.config = configRes;
    state.systemInfo = systemRes;
    state.devMode = statusRes?.dev_mode === true;
    if (state.devMode && dom.devTools) dom.devTools.classList.remove('hidden');
    const advancedSection = document.getElementById('settings-section-advanced');
    if (advancedSection) advancedSection.classList.toggle('hidden', !state.devMode);

    // If Minecraft is not found, show setup screen and wait
    if (statusRes?.setup_required) {
      await showSetup();
    }

    // Load versions and instances
    const [versionsRes, instancesRes] = await Promise.all([
      api('GET', '/versions'),
      api('GET', '/instances'),
    ]);
    state.versions = versionsRes.versions || [];
    state.instances = instancesRes.instances || [];
    state.lastInstanceId = instancesRes.last_instance_id || null;
    applyConfig(state.config);
    applySystemInfo(state.systemInfo);
    renderInstanceList();
    // Restore last selected instance
    if (state.lastInstanceId) {
      const remembered = state.instances.find(i => i.id === state.lastInstanceId);
      if (remembered) selectInstance(remembered, { silent: true });
    }
    if (state.config && !state.config.onboarding_done) showOnboarding();
    else if (Music.enabled) {
      const startMusic = () => {
        window.removeEventListener('pointerdown', startMusic, true);
        window.removeEventListener('keydown', startMusic, true);
        Music.play();
      };
      window.addEventListener('pointerdown', startMusic, { once: false, capture: true });
      window.addEventListener('keydown', startMusic, { once: false, capture: true });
    }
    watchVersions();
  } catch (err) {
    if (dom.versionList) dom.versionList.innerHTML = `<div class="loading-placeholder"><span style="color:var(--red)">Failed to connect</span><span style="color:var(--text-muted);font-size:10px">${err.message}</span></div>`;
  }

  bindEvents();
}

function applyConfig(cfg) {
  if (!cfg) return;
  if (cfg.username && dom.usernameInput) dom.usernameInput.value = cfg.username;
  if (cfg.max_memory_mb && dom.memorySlider) {
    const gb = cfg.max_memory_mb / 1024;
    dom.memorySlider.value = gb;
    if (dom.memoryValue) dom.memoryValue.textContent = fmtMem(gb);
  }
  // Restore theme from backend config if localStorage didn't have it
  if (cfg.theme && local.theme === 'obsidian' && cfg.theme !== 'obsidian') {
    applyTheme(cfg.theme, cfg.custom_hue ?? local.customHue, {
      silent: true,
      vibrancy: cfg.custom_vibrancy ?? local.customVibrancy,
      lightness: cfg.lightness ?? local.lightness,
    });
  }
  Music.applyConfig(cfg);
}

function applySystemInfo(info) {
  if (!info?.total_memory_mb) return;
  const totalGB = Math.floor(info.total_memory_mb / 1024);
  if (totalGB > 0 && dom.memorySlider) {
    dom.memorySlider.max = totalGB;
    const cur = parseFloat(dom.memorySlider.value);
    if (cur > totalGB) {
      dom.memorySlider.value = totalGB;
      if (dom.memoryValue) dom.memoryValue.textContent = fmtMem(totalGB);
    }
    updateMemoryRecText(parseFloat(dom.memorySlider.value), totalGB);
  }
}

function bindEvents() {
  bindButtonSounds();
  bindContextMenu();
  bindDeleteWizard();
  const activateSound = () => Sound.activate();
  window.addEventListener('pointerdown', activateSound, { once: true, capture: true });
  window.addEventListener('touchstart', activateSound, { once: true, capture: true });
  window.addEventListener('keydown', activateSound, { once: true, capture: true });

  dom.versionSearch?.addEventListener('input', (e) => {
    state.search = e.target.value;
    renderInstanceList();
  });

  $$('.filter-chips .chip[data-filter]').forEach(chip => {
    chip.addEventListener('click', () => {
      chip.parentElement.querySelectorAll('.chip').forEach(c => c.classList.remove('active'));
      chip.classList.add('active');
      state.filter = chip.dataset.filter;
      local.sidebarFilter = state.filter;
      saveLocalState();
      renderInstanceList();
    });
  });

  dom.memorySlider?.addEventListener('input', () => {
    const v = parseFloat(dom.memorySlider.value);
    if (dom.memoryValue) dom.memoryValue.textContent = fmtMem(v);
    updateMemoryRecText(v, state.systemInfo?.total_memory_mb ? Math.floor(state.systemInfo.total_memory_mb / 1024) : null);
    playSliderSound(v / parseFloat(dom.memorySlider.max || 16), 'memory');
  });

  dom.usernameInput?.addEventListener('blur', () => {
    const u = dom.usernameInput.value.trim();
    if (u && u !== state.config?.username) {
      api('PUT', '/config', { username: u });
      if (state.config) state.config.username = u;
    }
  });

  dom.launchBtn?.addEventListener('click', launchGame);
  dom.installBtn?.addEventListener('click', installVersion);
  dom.killBtn?.addEventListener('click', killGame);

  dom.logToggle?.addEventListener('click', (e) => {
    if (e.target.closest('.log-filter')) return;
    dom.logPanel?.classList.toggle('expanded');
    local.logExpanded = dom.logPanel?.classList.contains('expanded');
    saveLocalState();
  });

  dom.logFilter?.addEventListener('change', (e) => {
    setLogFilter(e.target.value);
  });

  // Log panel resize drag
  if (dom.logResize && dom.logPanel) {
    let dragging = false;
    dom.logResize.addEventListener('mousedown', (e) => {
      e.preventDefault();
      dragging = true;
      dom.logPanel.classList.add('resizing');
      const onMove = (ev) => {
        if (!dragging) return;
        const panelBottom = window.innerHeight;
        const newH = Math.max(100, Math.min(panelBottom - ev.clientY, window.innerHeight * 0.8));
        dom.logPanel.style.setProperty('--log-h', `${newH}px`);
      };
      const onUp = () => {
        dragging = false;
        dom.logPanel.classList.remove('resizing');
        const h = parseInt(getComputedStyle(dom.logPanel).height, 10);
        if (h > 50) { local.logHeight = h; saveLocalState(); }
        window.removeEventListener('mousemove', onMove);
        window.removeEventListener('mouseup', onUp);
      };
      window.addEventListener('mousemove', onMove);
      window.addEventListener('mouseup', onUp);
    });
  }

  dom.settingsBtn?.addEventListener('click', () => {
    if (state.currentPage === 'settings') closeSettings();
    else openSettings();
  });
  dom.settingsCancel?.addEventListener('click', closeSettings);
  dom.settingsSave?.addEventListener('click', saveSettings);
  dom.settingsContent?.addEventListener('scroll', syncSettingsSectionNav);
  dom.settingsNav?.querySelectorAll('.settings-nav-btn').forEach(btn => {
    btn.addEventListener('click', () => {
      Sound.ui('soft');
      const section = document.getElementById(btn.dataset.settingsTarget);
      section?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    });
  });
  dom.themePicker?.querySelectorAll('.theme-swatch').forEach(s => s.addEventListener('click', () => applyTheme(s.dataset.theme)));
  initColorField(dom.colorField, dom.colorFieldMarker,
    (hue, vibrancy) => {
      applyTheme('custom', hue, { silent: true, vibrancy });
      playSliderSound(hue / 360, 'hue');
    },
    () => Sound.ui('theme')
  );
  document.querySelectorAll('.lightness-slider').forEach(slider => {
    slider.addEventListener('input', () => {
      const lt = parseInt(slider.value, 10);
      applyTheme(local.theme, null, { silent: true, lightness: lt });
      playSliderSound(lt / 100, 'hue');
    });
    slider.addEventListener('change', () => Sound.ui('theme'));
  });
  document.getElementById('wcag-fix-btn')?.addEventListener('click', () => {
    const h = local.theme === 'custom' ? local.customHue : (PRESET_HUES[local.theme] || 140);
    const v = local.customVibrancy ?? 100;
    const fixed = findFixedLightness(h, v, local.lightness);
    applyTheme(local.theme, null, { lightness: fixed });
  });
  dom.shortcutList?.addEventListener('click', (e) => {
    const rec = e.target.closest('[data-sc-record]');
    if (rec) { startRecording(rec.dataset.scRecord); return; }
    const rst = e.target.closest('[data-sc-reset]');
    if (rst) {
      Shortcuts.reset(rst.dataset.scReset);
      local.shortcuts = Shortcuts._custom;
      saveLocalState();
      renderShortcutEditor();
      syncShortcutHints();
      Sound.ui('soft');
    }
  });
  dom.soundsToggle?.addEventListener('change', () => {
    const next = dom.soundsToggle.checked;
    if (next) {
      Sound.enabled = true;
      Sound.ui('theme');
    } else {
      Sound.ui('soft');
      setTimeout(() => { Sound.enabled = false; }, 40);
    }
    local.sounds = next;
    saveLocalState();
  });

  // Music controls
  dom.musicBtn?.addEventListener('click', () => {
    Music.toggle();
    Sound.ui(Music.enabled ? 'affirm' : 'soft');
  });
  dom.musicToggle?.addEventListener('change', () => {
    if (dom.musicToggle.checked !== Music.enabled) Music.toggle();
    Sound.ui(Music.enabled ? 'affirm' : 'soft');
  });
  dom.musicVolumeSlider?.addEventListener('input', () => {
    Music.setVolume(parseInt(dom.musicVolumeSlider.value, 10));
    Music.syncUI();
  });

  // Onboarding music buttons
  dom.obMusicYes?.addEventListener('click', () => {
    dom.obMusicYes?.classList.add('active');
    dom.obMusicNo?.classList.remove('active');
    Sound.ui('affirm');
  });
  dom.obMusicNo?.addEventListener('click', () => {
    dom.obMusicNo?.classList.add('active');
    dom.obMusicYes?.classList.remove('active');
    Sound.ui('soft');
  });

  dom.addVersionBtn?.addEventListener('click', openNewInstanceFlow);
  dom.emptyAddBtn?.addEventListener('click', openNewInstanceFlow);

  dom.onboardingNext?.addEventListener('click', onboardingNext);
  dom.onboardingBack?.addEventListener('click', () => { onboardingBack(); Sound.ui('soft'); });
  dom.onboardingMemorySlider?.addEventListener('input', () => {
    const v = parseFloat(dom.onboardingMemorySlider.value);
    if (dom.onboardingMemoryValue) dom.onboardingMemoryValue.textContent = fmtMem(v);
    const gb = state.systemInfo?.total_memory_mb ? Math.floor(state.systemInfo.total_memory_mb / 1024) : null;
    if (gb && dom.onboardingRec) dom.onboardingRec.textContent = v < 2 ? 'Low — may cause issues' : v > gb * 0.75 ? 'High — leave room for OS' : getMemoryRecommendation(gb).text;
    playSliderSound(v / parseFloat(dom.onboardingMemorySlider.max || 16), 'memory');
  });
  dom.obThemePresets?.querySelectorAll('.ob-theme-btn').forEach(btn => {
    btn.addEventListener('click', () => {
      dom.obThemePresets.querySelectorAll('.ob-theme-btn').forEach(b => b.classList.remove('active'));
      btn.classList.add('active');
      applyTheme(btn.dataset.obTheme);
    });
  });
  initColorField(dom.obColorField, dom.obColorFieldMarker,
    (hue, vibrancy) => {
      applyTheme('custom', hue, { silent: true, vibrancy });
      dom.obThemePresets?.querySelectorAll('.ob-theme-btn').forEach(b => b.classList.remove('active'));
      playSliderSound(hue / 360, 'hue');
    },
    () => Sound.ui('theme')
  );

  dom.devCleanup?.addEventListener('click', async () => {
    const ok = await showConfirm('Remove all installed versions and instances?\nInstance data (saves, mods) will be backed up.', { confirmText: 'Remove All', destructive: true });
    if (!ok) return;
    dom.devCleanup.disabled = true;
    dom.devCleanup.textContent = 'Working...';
    try {
      const res = await api('POST', '/dev/cleanup-versions');
      if (res.error) showError(res.error);
      else {
        appendLog('system', `Removed ${res.versions_removed} versions, ${res.instances_removed} instances`);
        state.versions = (await api('GET', '/versions')).versions || [];
        state.instances = [];
        state.selectedInstance = null;
        state.catalog = null;
        dom.versionDetail?.classList.add('hidden');
        dom.emptyState?.classList.remove('hidden');
        renderInstanceList();
      }
    } catch (err) {
      showError(err.message);
    }
    dom.devCleanup.disabled = false;
    dom.devCleanup.textContent = 'Cleanup All';
  });

  dom.devFlush?.addEventListener('click', async () => {
    const ok = await showConfirm('Delete all settings? App will restart.', { confirmText: 'Delete', destructive: true });
    if (!ok) return;
    try {
      await api('POST', '/dev/flush');
      localStorage.removeItem(STORAGE_KEY);
      location.reload();
    } catch (err) {
      showError(err.message);
    }
  });

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Control') { toggleShortcutHints(true); return; }
    if (handleRecordKey(e)) return;

    if (Shortcuts.matches(e, 'settings')) {
      e.preventDefault();
      if (state.currentPage === 'settings') { Sound.ui('soft'); closeSettings(); }
      else { Sound.ui('theme'); openSettings(); }
      return;
    }
    if (Shortcuts.matches(e, 'newInstance')) {
      e.preventDefault();
      if (state.currentPage === 'settings') closeSettings();
      setPage('launcher');
      openNewInstanceFlow();
      return;
    }
    if (Shortcuts.matches(e, 'launch')) {
      e.preventDefault();
      if (state.currentPage === 'settings') closeSettings();
      setPage('launcher');
      const selVer = state.selectedInstance ? state.versions.find(v => v.id === state.selectedInstance.version_id) : null;
      const inst = state.selectedInstance;
      if (selVer?.launchable && !state.launchingInstanceId && !state.runningSessions[inst?.id]) { Sound.ui('launchPress'); launchGame(); }
      else { Sound.ui('soft'); dom.launchBtn?.focus(); }
      return;
    }
    if (Shortcuts.matches(e, 'search')) {
      e.preventDefault();
      if (state.currentPage === 'settings') closeSettings();
      setPage('launcher');
      Sound.ui('soft');
      dom.versionSearch?.focus();
      dom.versionSearch?.select?.();
      return;
    }
    if (Shortcuts.matches(e, 'save') && state.currentPage === 'settings') {
      e.preventDefault();
      saveSettings();
      return;
    }
    if (Shortcuts.matches(e, 'close')) {
      // Close in priority order: dialog > context menu > delete wizard > new instance modal > settings
      const dialogOverlay = document.getElementById('dialog-overlay');
      const ctxMenu = document.getElementById('ctx-menu');
      const deleteModal = document.getElementById('delete-modal');
      const niModal = document.getElementById('new-instance-modal');
      if (dialogOverlay) dialogOverlay.remove();
      else if (ctxMenu && !ctxMenu.classList.contains('hidden')) hideContextMenu();
      else if (deleteModal && !deleteModal.classList.contains('hidden')) closeDeleteWizard();
      else if (niModal) { niModal.remove(); Sound.ui('soft'); }
      else if (state.currentPage === 'settings') closeSettings();
    }
    if (dom.onboarding && !dom.onboarding.classList.contains('hidden')) {
      if (e.key === 'Enter') { e.preventDefault(); onboardingNext(); }
      if (e.key === 'Backspace' && getObStep() > 1 && document.activeElement?.tagName !== 'INPUT') { e.preventDefault(); onboardingBack(); }
    }
  });
  document.addEventListener('keyup', (e) => {
    if (e.key === 'Control') toggleShortcutHints(false);
  });
  window.addEventListener('blur', () => toggleShortcutHints(false));
}

init();
