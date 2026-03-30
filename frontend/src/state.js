export const API = '/api/v1';
export const STORAGE_KEY = 'croopor_ui';

export const PRESET_HUES = { obsidian: 140, deepslate: 215, nether: 15, end: 268, birch: 100 };
export const LOGO_BASE_HUE = 106;

export const defaults = { theme: 'obsidian', customHue: 140, customVibrancy: 100, lightness: 0, logExpanded: false, logHeight: 0, collapsedGroups: {}, sidebarFilter: 'all', sounds: true, shortcuts: {} };

export function loadLocalState() {
  try { const r = localStorage.getItem(STORAGE_KEY); return r ? { ...defaults, ...JSON.parse(r) } : { ...defaults }; } catch { return { ...defaults }; }
}

export const local = loadLocalState();

export function saveLocalState() {
  try { localStorage.setItem(STORAGE_KEY, JSON.stringify(local)); } catch {}
}

export const state = {
  instances: [], versions: [], config: null, systemInfo: null, devMode: false,
  selectedInstance: null, selectedVersion: null,
  installEventSource: null,
  logLines: 0, filter: 'all',
  search: '', catalog: null,
  runningSessions: {}, launchingInstanceId: null,
  installQueue: [], activeInstall: null,
  currentPage: 'launcher',
  lastInstanceId: null,
};

export const dom = {};
export const $ = (sel) => document.querySelector(sel);
export const $$ = (sel) => document.querySelectorAll(sel);

export function cacheDom() {
  const ids = [
    'version-list', 'version-search', 'empty-state', 'empty-title', 'empty-sub', 'empty-add-btn',
    'center-panel', 'page-stack', 'launcher-view', 'settings-view', 'settings-content', 'settings-nav', 'sidebar-launcher-panel', 'sidebar-settings-panel',
    'version-detail', 'detail-id', 'detail-badge', 'detail-props',
    'launch-area', 'launch-btn', 'launching-area', 'launch-ascii', 'launch-seq-version',
    'running-area', 'running-ascii', 'running-version', 'running-pid', 'running-uptime', 'kill-btn',
    'not-launchable', 'not-launchable-text',
    'install-area', 'install-text', 'install-btn', 'install-progress', 'progress-fill', 'progress-text',
    'username-input', 'memory-slider', 'memory-value', 'memory-rec',
    'log-panel', 'log-toggle', 'log-content', 'log-lines', 'log-count', 'log-resize', 'log-filter',
    'settings-btn', 'settings-cancel', 'settings-save',
    'setting-java-path', 'setting-width', 'setting-height', 'java-runtimes', 'jvm-preset-group',
    'theme-picker', 'color-field', 'color-field-marker', 'lightness-slider', 'sounds-toggle', 'shortcut-list',
    'add-version-btn',
    'onboarding', 'onboarding-step-1', 'onboarding-step-2', 'onboarding-step-3', 'onboarding-step-4', 'onboarding-step-5',
    'onboarding-username', 'onboarding-ram-info', 'onboarding-memory-slider', 'onboarding-memory-value', 'onboarding-rec',
    'onboarding-back', 'onboarding-next',
    'dot-1', 'dot-2', 'dot-3', 'dot-4', 'dot-5',
    'music-btn', 'music-eq', 'music-toggle', 'music-volume-slider', 'music-volume-value', 'music-volume-row',
    'ob-music-yes', 'ob-music-no',
    'ob-theme-presets', 'ob-color-field', 'ob-color-field-marker', 'ob-lightness-slider',
    'dev-tools', 'dev-cleanup', 'dev-flush',
    'setup-overlay', 'setup-path-input', 'setup-path-error', 'setup-browse-btn', 'setup-use-btn',
    'setup-new-path', 'setup-init-btn',
  ];
  ids.forEach(id => { dom[id.replace(/-([a-z0-9])/g, (_, c) => c.toUpperCase())] = document.getElementById(id); });
}
