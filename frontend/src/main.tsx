import { render } from 'preact';
import { local, STORAGE_KEY, saveLocalState, PRESET_HUES } from './state';
import { byId, $$ } from './dom';
import {
  appVersion, bootstrapError, bootstrapState, collapsedGroups, config, currentPage,
  devMode, catalog, instances, lastInstanceId, launchState, runningSessions, searchQuery,
  selectedInstance, selectedInstanceId, selectedVersion, sidebarFilter, systemInfo, versions,
} from './store';
import { api } from './api';
import { Sound, bindButtonSounds, playSliderSound } from './sound';
import { App } from './components/App';
import { Music } from './music';
import { applyTheme, initColorField } from './theme';
import { Shortcuts, syncShortcutHints, handleRecordKey } from './shortcuts';
import { fmtMem, showError, appendLog, setLogFilter, setPage, toggleShortcutHints, getMemoryRecommendation, updateMemoryRecText } from './utils';
import { watchVersions } from './sidebar';
import { selectInstance } from './actions';
import { launchGame } from './launch';
import { openSettings, closeSettings, saveSettings, syncSettingsSectionNav } from './settings';
import { openNewInstanceFlow, closeNewInstanceFlow, showSetup, showOnboarding, onboardingStep, onboardingNext, onboardingBack, getObStep, finishOnboarding } from './modals';
import { hideContextMenu, bindContextMenu } from './context-menu';
import { closeDeleteWizard, bindDeleteWizard } from './delete-wizard';
import { dismissDialog, showConfirm } from './dialogs';
import { getNativeAppVersion } from './native';

async function init(): Promise<void> {
  render(<App />, document.getElementById('app')!);
  const nativeVersion = await getNativeAppVersion();
  if (nativeVersion) appVersion.value = nativeVersion;
  Shortcuts.load(local.shortcuts);
  applyTheme(local.theme, local.customHue, { silent: true, vibrancy: local.customVibrancy, lightness: local.lightness });
  Sound.enabled = local.sounds;
  Sound.warmup();
  const logPanel = byId<HTMLElement>('log-panel');
  if (local.logExpanded) logPanel?.classList.add('expanded');
  if (local.logHeight && logPanel) logPanel.style.setProperty('--log-h', `${local.logHeight}px`);
  sidebarFilter.value = local.sidebarFilter;
  $$<HTMLElement>('.filter-chips .chip[data-filter]').forEach((c: HTMLElement) => c.classList.toggle('active', c.dataset.filter === sidebarFilter.value));
  setPage('launcher');

  try {
    const [configRes, systemRes, statusRes] = await Promise.all([
      api('GET', '/config'),
      api('GET', '/system').catch(() => null),
      api('GET', '/status').catch(() => null),
    ]);
    config.value = configRes;
    systemInfo.value = systemRes;
    devMode.value = statusRes?.dev_mode === true;

    // If Minecraft is not found, show setup screen and wait
    if (statusRes?.setup_required) {
      await showSetup();
    }

    // Load versions and instances
    const [versionsRes, instancesRes] = await Promise.all([
      api('GET', '/versions'),
      api('GET', '/instances'),
    ]);
    versions.value = versionsRes.versions || [];
    instances.value = instancesRes.instances || [];
    lastInstanceId.value = instancesRes.last_instance_id || null;
    applyConfig(config.value);
    applySystemInfo(systemInfo.value);
    // Initialize collapsed groups signal from localStorage
    collapsedGroups.value = { ...local.collapsedGroups };
    syncShortcutHints();
    Music.syncUI();
    bootstrapError.value = null;
    bootstrapState.value = 'ready';
    // Restore last selected instance
    if (lastInstanceId.value) {
      const remembered = instances.value.find((instance) => instance.id === lastInstanceId.value);
      if (remembered) selectInstance(remembered.id);
    }
    if (config.value && !config.value.onboarding_done) showOnboarding();
    else if (Music.enabled) {
      const startMusic = (): void => {
        window.removeEventListener('pointerdown', startMusic, true);
        window.removeEventListener('keydown', startMusic, true);
        Music.play();
      };
      window.addEventListener('pointerdown', startMusic, { once: false, capture: true });
      window.addEventListener('keydown', startMusic, { once: false, capture: true });
    }
    watchVersions();
  } catch (err: unknown) {
    bootstrapError.value = (err as Error).message;
    bootstrapState.value = 'error';
  }

  bindEvents();
}

function applyConfig(cfg: any): void {
  if (!cfg) return;
  const usernameInput = byId<HTMLInputElement>('username-input');
  const memorySlider = byId<HTMLInputElement>('memory-slider');
  const memoryValue = byId<HTMLElement>('memory-value');
  if (cfg.username && usernameInput) usernameInput.value = cfg.username;
  if (cfg.max_memory_mb && memorySlider) {
    const gb: number = cfg.max_memory_mb / 1024;
    memorySlider.value = String(gb);
    if (memoryValue) memoryValue.textContent = fmtMem(gb);
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

function applySystemInfo(info: any): void {
  if (!info?.total_memory_mb) return;
  const memorySlider = byId<HTMLInputElement>('memory-slider');
  const memoryValue = byId<HTMLElement>('memory-value');
  const totalGB: number = Math.floor(info.total_memory_mb / 1024);
  if (totalGB > 0 && memorySlider) {
    memorySlider.max = String(totalGB);
    const cur: number = parseFloat(memorySlider.value);
    if (cur > totalGB) {
      memorySlider.value = String(totalGB);
      if (memoryValue) memoryValue.textContent = fmtMem(totalGB);
    }
    updateMemoryRecText(parseFloat(memorySlider.value), totalGB);
  }
}

function bindEvents(): void {
  const versionSearch = byId<HTMLInputElement>('version-search');
  const memorySlider = byId<HTMLInputElement>('memory-slider');
  const memoryValue = byId<HTMLElement>('memory-value');
  const usernameInput = byId<HTMLInputElement>('username-input');
  const logToggle = byId<HTMLElement>('log-toggle');
  const logPanel = byId<HTMLElement>('log-panel');
  const logFilter = byId<HTMLSelectElement>('log-filter');
  const logResize = byId<HTMLElement>('log-resize');
  const settingsBtn = byId<HTMLElement>('settings-btn');
  const settingsCancel = byId<HTMLElement>('settings-cancel');
  const settingsSave = byId<HTMLElement>('settings-save');
  const settingsContent = byId<HTMLElement>('settings-content');
  const settingsNav = byId<HTMLElement>('settings-nav');
  const musicBtn = byId<HTMLElement>('music-btn');
  const obMusicYes = byId<HTMLElement>('ob-music-yes');
  const obMusicNo = byId<HTMLElement>('ob-music-no');
  const addVersionBtn = byId<HTMLElement>('add-version-btn');
  const emptyAddBtn = byId<HTMLElement>('empty-add-btn');
  const onboardingNextBtn = byId<HTMLElement>('onboarding-next');
  const onboardingBackBtn = byId<HTMLElement>('onboarding-back');
  const onboardingMemorySlider = byId<HTMLInputElement>('onboarding-memory-slider');
  const onboardingMemoryValue = byId<HTMLElement>('onboarding-memory-value');
  const onboardingRec = byId<HTMLElement>('onboarding-rec');
  const obThemePresets = byId<HTMLElement>('ob-theme-presets');
  const obColorField = byId<HTMLElement>('ob-color-field');
  const obColorFieldMarker = byId<HTMLElement>('ob-color-field-marker');
  const devCleanup = byId<HTMLButtonElement>('dev-cleanup');
  const devFlush = byId<HTMLElement>('dev-flush');

  bindButtonSounds();
  bindContextMenu();
  bindDeleteWizard();
  const activateSound = (): void => { Sound.activate(); };
  window.addEventListener('pointerdown', activateSound, { once: true, capture: true });
  window.addEventListener('touchstart', activateSound, { once: true, capture: true });
  window.addEventListener('keydown', activateSound, { once: true, capture: true });

  versionSearch?.addEventListener('input', (e: Event) => {
    searchQuery.value = (e.target as HTMLInputElement).value;
  });

  $$<HTMLElement>('.filter-chips .chip[data-filter]').forEach((chip: HTMLElement) => {
    chip.addEventListener('click', () => {
      chip.parentElement!.querySelectorAll('.chip').forEach((c: Element) => c.classList.remove('active'));
      chip.classList.add('active');
      sidebarFilter.value = chip.dataset.filter || 'all';
      local.sidebarFilter = sidebarFilter.value;
      saveLocalState();
    });
  });

  memorySlider?.addEventListener('input', () => {
    const slider = memorySlider;
    const v: number = parseFloat(slider.value);
    if (memoryValue) memoryValue.textContent = fmtMem(v);
    updateMemoryRecText(v, systemInfo.value?.total_memory_mb ? Math.floor(systemInfo.value.total_memory_mb / 1024) : 0);
    playSliderSound(v / parseFloat(slider.max || '16'), 'memory');
  });

  usernameInput?.addEventListener('blur', () => {
    const u: string = usernameInput.value.trim();
    if (u && u !== config.value?.username) {
      api('PUT', '/config', { username: u });
      if (config.value) config.value = { ...config.value, username: u };
    }
  });

  // Launch/Install/Kill buttons are now handled by the Preact <ActionArea> component.

  logToggle?.addEventListener('click', (e: MouseEvent) => {
    if ((e.target as HTMLElement).closest('.log-filter')) return;
    logPanel?.classList.toggle('expanded');
    local.logExpanded = !!logPanel?.classList.contains('expanded');
    saveLocalState();
  });

  logFilter?.addEventListener('change', (e: Event) => {
    setLogFilter((e.target as HTMLSelectElement).value);
  });

  // Log panel resize drag
  if (logResize && logPanel) {
    let dragging = false;
    logResize.addEventListener('mousedown', (e: MouseEvent) => {
      e.preventDefault();
      dragging = true;
      logPanel.classList.add('resizing');
      const onMove = (ev: MouseEvent): void => {
        if (!dragging) return;
        const panelBottom: number = window.innerHeight;
        const newH: number = Math.max(100, Math.min(panelBottom - ev.clientY, window.innerHeight * 0.8));
        logPanel.style.setProperty('--log-h', `${newH}px`);
      };
      const onUp = (): void => {
        dragging = false;
        logPanel.classList.remove('resizing');
        const h: number = parseInt(getComputedStyle(logPanel).height, 10);
        if (h > 50) { local.logHeight = h; saveLocalState(); }
        window.removeEventListener('mousemove', onMove);
        window.removeEventListener('mouseup', onUp);
      };
      window.addEventListener('mousemove', onMove);
      window.addEventListener('mouseup', onUp);
    });
  }

  settingsBtn?.addEventListener('click', () => {
    if (currentPage.value === 'settings') closeSettings();
    else openSettings();
  });
  settingsCancel?.addEventListener('click', () => closeSettings());
  settingsSave?.addEventListener('click', () => saveSettings());
  settingsContent?.addEventListener('scroll', () => syncSettingsSectionNav());
  settingsNav?.querySelectorAll<HTMLElement>('.settings-nav-btn').forEach((btn: HTMLElement) => {
    btn.addEventListener('click', () => {
      Sound.ui('soft');
      const section: HTMLElement | null = document.getElementById(btn.dataset.settingsTarget!);
      section?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    });
  });

  // Music controls
  musicBtn?.addEventListener('click', () => {
    Music.toggle();
    Sound.ui(Music.enabled ? 'affirm' : 'soft');
  });

  // Onboarding music buttons
  obMusicYes?.addEventListener('click', () => {
    obMusicYes.classList.add('active');
    obMusicNo?.classList.remove('active');
    Sound.ui('affirm');
  });
  obMusicNo?.addEventListener('click', () => {
    obMusicNo.classList.add('active');
    obMusicYes?.classList.remove('active');
    Sound.ui('soft');
  });

  addVersionBtn?.addEventListener('click', () => openNewInstanceFlow());
  emptyAddBtn?.addEventListener('click', () => openNewInstanceFlow());

  onboardingNextBtn?.addEventListener('click', () => onboardingNext());
  onboardingBackBtn?.addEventListener('click', () => { onboardingBack(); Sound.ui('soft'); });
  onboardingMemorySlider?.addEventListener('input', () => {
    const slider = onboardingMemorySlider;
    const v: number = parseFloat(slider.value);
    if (onboardingMemoryValue) onboardingMemoryValue.textContent = fmtMem(v);
    const gb = systemInfo.value?.total_memory_mb ? Math.floor(systemInfo.value.total_memory_mb / 1024) : null;
    if (gb && onboardingRec) onboardingRec.textContent = v < 2 ? 'Low — may cause issues' : v > gb * 0.75 ? 'High — leave room for OS' : getMemoryRecommendation(gb).text;
    playSliderSound(v / parseFloat(slider.max || '16'), 'memory');
  });
  obThemePresets?.querySelectorAll<HTMLElement>('.ob-theme-btn').forEach((btn: HTMLElement) => {
    btn.addEventListener('click', () => {
      obThemePresets.querySelectorAll('.ob-theme-btn').forEach((b: Element) => b.classList.remove('active'));
      btn.classList.add('active');
      applyTheme(btn.dataset.obTheme!, null);
    });
  });
  initColorField(obColorField, obColorFieldMarker,
    (hue: number, vibrancy: number) => {
      applyTheme('custom', hue, { silent: true, vibrancy });
      obThemePresets?.querySelectorAll('.ob-theme-btn').forEach((b: Element) => b.classList.remove('active'));
      playSliderSound(hue / 360, 'hue');
    },
    () => Sound.ui('theme')
  );

  devCleanup?.addEventListener('click', async () => {
    const ok: boolean = await showConfirm('Remove all installed versions and instances?\nInstance data (saves, mods) will be backed up.', { confirmText: 'Remove All', destructive: true });
    if (!ok) return;
    const btn = devCleanup;
    btn.disabled = true;
    btn.textContent = 'Working...';
    try {
      const res: any = await api('POST', '/dev/cleanup-versions');
      if (res.error) showError(res.error);
      else {
        appendLog('system', `Removed ${res.versions_removed} versions, ${res.instances_removed} instances`);
        versions.value = (await api('GET', '/versions')).versions || [];
        instances.value = [];
        selectedInstanceId.value = null;
        catalog.value = null;
      }
    } catch (err: unknown) {
      showError((err as Error).message);
    }
    btn.disabled = false;
    btn.textContent = 'Cleanup All';
  });

  devFlush?.addEventListener('click', async () => {
    const ok: boolean = await showConfirm('Delete all settings? App will restart.', { confirmText: 'Delete', destructive: true });
    if (!ok) return;
    try {
      await api('POST', '/dev/flush');
      localStorage.removeItem(STORAGE_KEY);
      location.reload();
    } catch (err: unknown) {
      showError((err as Error).message);
    }
  });

  document.addEventListener('keydown', (e: KeyboardEvent) => {
    if (e.key === 'Control') { toggleShortcutHints(true); return; }
    if (handleRecordKey(e)) return;

    if (Shortcuts.matches(e, 'settings')) {
      e.preventDefault();
      if (currentPage.value === 'settings') { Sound.ui('soft'); closeSettings(); }
      else { Sound.ui('theme'); openSettings(); }
      return;
    }
    if (Shortcuts.matches(e, 'newInstance')) {
      e.preventDefault();
      if (currentPage.value === 'settings') closeSettings();
      setPage('launcher');
      openNewInstanceFlow();
      return;
    }
    if (Shortcuts.matches(e, 'launch')) {
      e.preventDefault();
      if (currentPage.value === 'settings') closeSettings();
      setPage('launcher');
      const inst = selectedInstance.value;
      const selVer = selectedVersion.value;
      if (selVer?.launchable && launchState.value.status !== 'preparing' && !runningSessions.value[inst?.id || '']) { Sound.ui('launchPress'); launchGame(); }
      else { Sound.ui('soft'); byId<HTMLElement>('launch-btn')?.focus(); }
      return;
    }
    if (Shortcuts.matches(e, 'search')) {
      e.preventDefault();
      if (currentPage.value === 'settings') closeSettings();
      setPage('launcher');
      Sound.ui('soft');
      versionSearch?.focus();
      versionSearch?.select?.();
      return;
    }
    if (Shortcuts.matches(e, 'save') && currentPage.value === 'settings') {
      e.preventDefault();
      saveSettings();
      return;
    }
    if (Shortcuts.matches(e, 'close')) {
      // Close in priority order: dialog > context menu > delete wizard > new instance modal > settings
      const dialogOverlay: HTMLElement | null = document.getElementById('dialog-overlay');
      const ctxMenu: HTMLElement | null = document.getElementById('ctx-menu');
      const deleteModal: HTMLElement | null = document.getElementById('delete-modal');
      if (dialogOverlay) dismissDialog();
      else if (ctxMenu && !ctxMenu.classList.contains('hidden')) hideContextMenu();
      else if (deleteModal && !deleteModal.classList.contains('hidden')) closeDeleteWizard();
      else if (document.getElementById('new-instance-modal')) closeNewInstanceFlow();
      else if (currentPage.value === 'settings') closeSettings();
    }
    if (byId<HTMLElement>('onboarding') && !byId<HTMLElement>('onboarding')?.classList.contains('hidden')) {
      if (e.key === 'Enter') { e.preventDefault(); onboardingNext(); }
      if (e.key === 'Backspace' && getObStep() > 1 && document.activeElement?.tagName !== 'INPUT') { e.preventDefault(); onboardingBack(); }
    }
  });
  document.addEventListener('keyup', (e: KeyboardEvent) => {
    if (e.key === 'Control') toggleShortcutHints(false);
  });
  window.addEventListener('blur', () => toggleShortcutHints(false));
}

init();
