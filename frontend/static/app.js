// ═══════════════════════════════════════════
// Croopor — Frontend Application
// ═══════════════════════════════════════════

const API = '/api/v1';
const STORAGE_KEY = 'croopor_ui';

// ── Local UI State ──

const PRESET_HUES = { obsidian: 140, deepslate: 215, nether: 15, end: 268, birch: 100 };
const LOGO_BASE_HUE = 106;

const SHORTCUT_DEFAULTS = {
  settings:   { key: ',', ctrl: true, desc: 'Open or close settings' },
  search:     { key: 'f', ctrl: true, desc: 'Focus version search' },
  addVersion: { key: 'n', ctrl: true, desc: 'Add a new version' },
  launch:     { key: 'Enter', ctrl: true, desc: 'Launch selected version' },
  save:       { key: 's', ctrl: true, desc: 'Save settings' },
  close:      { key: 'Escape', ctrl: false, desc: 'Close dialogs' },
};

const Shortcuts = {
  _custom: {},
  load(stored) { this._custom = stored || {}; },
  get(action) { return this._custom[action] || SHORTCUT_DEFAULTS[action]; },
  set(action, binding) { this._custom[action] = binding; },
  reset(action) { delete this._custom[action]; },
  all() { return Object.keys(SHORTCUT_DEFAULTS); },
  matches(e, action) {
    const b = this.get(action);
    if (!b) return false;
    const key = b.key.length === 1 ? b.key.toLowerCase() : b.key;
    const eKey = e.key.length === 1 ? e.key.toLowerCase() : e.key;
    return eKey === key && !!e.ctrlKey === !!b.ctrl && !!e.shiftKey === !!b.shift && !!e.altKey === !!b.alt;
  },
  format(action) {
    const b = this.get(action);
    if (!b) return '';
    const parts = [];
    if (b.ctrl) parts.push('Ctrl');
    if (b.shift) parts.push('Shift');
    if (b.alt) parts.push('Alt');
    const k = b.key === ' ' ? 'Space' : b.key === ',' ? ',' : b.key.length === 1 ? b.key.toUpperCase() : b.key;
    parts.push(k);
    return parts.join('+');
  },
};

const defaults = { theme: 'obsidian', customHue: 140, customVibrancy: 100, lightness: 0, logExpanded: false, collapsedGroups: {}, sidebarFilter: 'all', sounds: true, shortcuts: {} };
function loadLocalState() { try { const r = localStorage.getItem(STORAGE_KEY); return r ? { ...defaults, ...JSON.parse(r) } : { ...defaults }; } catch { return { ...defaults }; } }
function saveLocalState() { try { localStorage.setItem(STORAGE_KEY, JSON.stringify(local)); } catch {} }
const local = loadLocalState();
Shortcuts.load(local.shortcuts);

// ── Sound Engine ──

const Sound = {
  ctx: null,
  enabled: true,
  preloadPromise: null,
  spriteBuffer: null,
  spriteMap: null,
  customBuffers: new Map(),
  init() {
    if (this.ctx) return this.ctx;
    try {
      this.ctx = new (window.AudioContext || window.webkitAudioContext)();
    } catch {}
    return this.ctx;
  },
  activate() {
    this.init();
    this.preload();
    if (this.ctx?.state === 'suspended') this.ctx.resume().catch(() => {});
  },
  preload() {
    if (this.preloadPromise) return this.preloadPromise;
    this.init();
    if (!this.ctx) return Promise.resolve();
    this.preloadPromise = (async () => {
      try {
        const [manifestRes, spriteRes, launchRes] = await Promise.all([
          fetch('sounds/snd01/audioSprite.json'),
          fetch('sounds/snd01/audioSprite.mp3'),
          fetch('sounds/launch.ogg'),
        ]);
        const manifest = await manifestRes.json();
        const [spriteArray, launchArray] = await Promise.all([spriteRes.arrayBuffer(), launchRes.arrayBuffer()]);
        this.spriteMap = manifest.spritemap || {};
        this.spriteBuffer = await this.ctx.decodeAudioData(spriteArray.slice(0));
        this.customBuffers.set('launchSuccess', await this.ctx.decodeAudioData(launchArray.slice(0)));
      } catch {}
    })();
    return this.preloadPromise;
  },
  async warmup() {
    this.activate();
    try { await this.preload(); } catch {}
  },
  playBuffer(buffer, options = {}) {
    if (!buffer || !this.ctx) return false;
    const {
      when = 0,
      volume = 0.22,
      playbackRate = 1,
      offset = 0,
      duration = null,
    } = options;
    try {
      const source = this.ctx.createBufferSource();
      const gain = this.ctx.createGain();
      source.buffer = buffer;
      source.playbackRate.setValueAtTime(playbackRate, this.ctx.currentTime);
      gain.gain.setValueAtTime(Math.max(0.0001, volume), this.ctx.currentTime + when);
      source.connect(gain);
      gain.connect(this.ctx.destination);
      const startAt = this.ctx.currentTime + when;
      if (duration != null) source.start(startAt, offset, duration);
      else source.start(startAt, offset);
      return true;
    } catch {
      return false;
    }
  },
  playSprite(name, options = {}) {
    const entry = this.spriteMap?.[name];
    if (!entry || !this.spriteBuffer) return false;
    return this.playBuffer(this.spriteBuffer, {
      offset: entry.start,
      duration: Math.max(0.01, entry.end - entry.start),
      ...options,
    });
  },
  randomFrom(keys) {
    return keys[Math.floor(Math.random() * keys.length)];
  },
  playKind(kind, value = 0.5) {
    switch (kind) {
      case 'soft':
        return this.playSprite('tap_01', { volume: 0.18 });
      case 'bright':
        return this.playSprite(this.randomFrom(['swipe', 'swipe_01', 'swipe_02', 'swipe_03', 'swipe_04', 'swipe_05']), { volume: 0.22 });
      case 'affirm':
        return this.playSprite('button', { volume: 0.24 });
      case 'theme':
        return this.playSprite('transition_up', { volume: 0.26 });
      case 'slider':
        return this.playSprite('select', { volume: 0.15, playbackRate: 0.93 + (value * 0.16) });
      case 'memory':
        return this.playSprite('select', { volume: 0.18, playbackRate: 0.86 + (value * 0.12) });
      case 'launchPress':
        return this.playSprite('button', { volume: 0.3, playbackRate: 0.96 });
      case 'launchSuccess':
        return this.playBuffer(this.customBuffers.get('launchSuccess'), { volume: 0.38 }) || this.playSprite('celebration', { volume: 0.28 });
      case 'click':
      default:
        return this.playSprite(this.randomFrom(['tap_01', 'tap_02', 'tap_03', 'tap_04', 'tap_05']), { volume: 0.17 });
    }
  },
  tone(freq, duration, options = {}) {
    if (!this.enabled) return;
    this.init();
    if (!this.ctx) return;
    const {
      type = 'triangle',
      volume = 0.035,
      when = 0,
      attack = 0.008,
      release = 0.09,
      detune = 0,
      endFreq = null,
    } = options;
    try {
      const now = this.ctx.currentTime + when;
      const osc = this.ctx.createOscillator();
      const gain = this.ctx.createGain();
      osc.type = type;
      osc.frequency.setValueAtTime(freq, now);
      osc.detune.setValueAtTime(detune, now);
      if (endFreq) osc.frequency.exponentialRampToValueAtTime(endFreq, now + duration);
      gain.gain.setValueAtTime(0.0001, now);
      gain.gain.exponentialRampToValueAtTime(volume, now + attack);
      gain.gain.exponentialRampToValueAtTime(0.0001, now + duration + release);
      osc.connect(gain);
      gain.connect(this.ctx.destination);
      osc.start(now);
      osc.stop(now + duration + release + 0.01);
    } catch {}
  },
  sequence(notes) { notes.forEach(note => this.tone(note.freq, note.duration, note)); },
  ui(kind, value = 0.5) {
    if (!this.enabled) return;
    this.activate();
    if (this.playKind(kind, value)) return;
    switch (kind) {
      case 'soft':
        this.sequence([
          { freq: 340, duration: 0.024, volume: 0.013, type: 'sine' },
          { freq: 430, duration: 0.03, volume: 0.014, when: 0.015, type: 'triangle' },
        ]);
        break;
      case 'bright':
        this.sequence([
          { freq: 620, duration: 0.024, volume: 0.022, type: 'triangle' },
          { freq: 930, duration: 0.045, volume: 0.02, when: 0.018, type: 'sine' },
        ]);
        break;
      case 'affirm':
        this.sequence([
          { freq: 480, duration: 0.035, volume: 0.022, type: 'triangle' },
          { freq: 720, duration: 0.055, volume: 0.024, when: 0.024, type: 'triangle' },
          { freq: 960, duration: 0.09, volume: 0.018, when: 0.055, type: 'sine' },
        ]);
        break;
      case 'theme':
        this.sequence([
          { freq: 392, duration: 0.028, volume: 0.016, type: 'sine' },
          { freq: 587.33, duration: 0.05, volume: 0.02, when: 0.016, type: 'triangle' },
          { freq: 783.99, duration: 0.085, volume: 0.022, when: 0.04, type: 'triangle' },
          { freq: 1174.66, duration: 0.08, volume: 0.012, when: 0.085, type: 'sine' },
        ]);
        break;
      case 'slider': {
        const freq = 460 + (value * 360);
        this.sequence([{ freq, duration: 0.02, volume: 0.012, type: 'triangle', endFreq: freq * 1.05 }]);
        break;
      }
      case 'memory': {
        const freq = 150 + (value * 160);
        this.sequence([
          { freq, duration: 0.024, volume: 0.018, type: 'sine', endFreq: freq * 1.03 },
          { freq: freq * 1.5, duration: 0.03, volume: 0.009, when: 0.008, type: 'triangle' },
        ]);
        break;
      }
      case 'launchPress':
        this.sequence([
          { freq: 220, duration: 0.05, volume: 0.018, type: 'triangle' },
          { freq: 293.66, duration: 0.055, volume: 0.022, when: 0.028, type: 'triangle' },
          { freq: 440, duration: 0.07, volume: 0.016, when: 0.055, type: 'sine' },
        ]);
        break;
      case 'launchSuccess':
        this.sequence([
          { freq: 196, duration: 0.16, volume: 0.013, type: 'sine' },
          { freq: 392, duration: 0.07, volume: 0.022, when: 0.02, type: 'triangle' },
          { freq: 523.25, duration: 0.085, volume: 0.026, when: 0.085, type: 'triangle' },
          { freq: 659.25, duration: 0.11, volume: 0.028, when: 0.15, type: 'triangle' },
          { freq: 783.99, duration: 0.18, volume: 0.026, when: 0.215, type: 'triangle' },
          { freq: 1174.66, duration: 0.34, volume: 0.014, when: 0.25, type: 'sine' },
        ]);
        break;
      default:
        this.sequence([
          { freq: 520, duration: 0.02, volume: 0.015, type: 'triangle' },
          { freq: 690, duration: 0.028, volume: 0.012, when: 0.014, type: 'sine' },
        ]);
    }
  },
  tick() { this.ui('click'); },
};

// ── Text Scramble Effect ──

const SCRAMBLE_CHARS = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-';
const scrambleTimers = new Map();

function scrambleText(el, text, duration) {
  if (!el) return;
  if (scrambleTimers.has(el)) clearInterval(scrambleTimers.get(el));
  const steps = 7;
  const interval = (duration || 280) / steps;
  let step = 0;
  const id = setInterval(() => {
    step++;
    const reveal = Math.floor((step / steps) * text.length);
    let out = '';
    for (let i = 0; i < text.length; i++) {
      if (text[i] === ' ') out += ' ';
      else if (i < reveal) out += text[i];
      else out += SCRAMBLE_CHARS[Math.floor(Math.random() * SCRAMBLE_CHARS.length)];
    }
    el.textContent = out;
    if (step >= steps) { clearInterval(id); scrambleTimers.delete(el); el.textContent = text; }
  }, interval);
  scrambleTimers.set(el, id);
}

// ── Theme Engine ──

function generateThemeFromHue(hue, vibrancy, lightness) {
  const v = (vibrancy != null ? vibrancy : 100) / 100;
  const l = (lightness != null ? lightness : 0) / 100; // 0=dark, 1=light
  const baseSat = ((hue >= 0 && hue < 60) || hue >= 300 ? 18 : 15);
  const s = Math.round(baseSat * v);
  // lerp helper
  const mix = (dark, light) => Math.round(dark + (light - dark) * l);
  const mixF = (dark, light) => +(dark + (light - dark) * l).toFixed(2);

  const bgDeepL = mix(5, 90), bgL = mix(7, 94);
  const s0L = mix(9.5, 97), s1L = mix(12, 92), s2L = mix(15.5, 88), s3L = mix(19, 83);
  const bgS = Math.max(0, s), bgS2 = Math.max(0, s - 3), bgS3 = Math.max(0, s - 5);
  const accentL = mix(58, 42), accentDimL = mix(44, 34);
  const accentS = l > 0.5 ? Math.round(55 + v * 15) : 65;
  const accentDimS = l > 0.5 ? Math.round(45 + v * 15) : 55;
  const textL = mix(86, 16), textDimL = mix(52, 42), textMutedL = mix(34, 60);
  const textS = Math.round(mix(8 * v, 6 + v * 4));
  const borderL = mix(14, 84), borderHoverL = mix(24, 76);
  const borderS = Math.max(0, mix(s - 4, s - 2)), borderHoverS = Math.max(0, mix(s - 2, s));
  const shadowA = mixF(0.5, 0.08);

  return {
    '--bg-deep': `hsl(${hue},${bgS}%,${bgDeepL}%)`, '--bg': `hsl(${hue},${bgS2}%,${bgL}%)`,
    '--surface-0': `hsl(${hue},${bgS3}%,${s0L}%)`, '--surface-1': `hsl(${hue},${bgS3}%,${s1L}%)`,
    '--surface-2': `hsl(${hue},${bgS3}%,${s2L}%)`, '--surface-3': `hsl(${hue},${bgS3}%,${s3L}%)`,
    '--accent': `hsl(${hue},${accentS}%,${accentL}%)`, '--accent-dim': `hsl(${hue},${accentDimS}%,${accentDimL}%)`,
    '--accent-glow': `hsla(${hue},${accentS}%,${accentL}%,0.12)`, '--accent-glow-strong': `hsla(${hue},${accentS}%,${accentL}%,${mixF(0.28, 0.22)})`,
    '--text': `hsl(${hue},${textS}%,${textL}%)`, '--text-dim': `hsl(${hue},${textS}%,${textDimL}%)`,
    '--text-muted': `hsl(${hue},${textS}%,${textMutedL}%)`,
    '--border': `hsl(${hue},${borderS}%,${borderL}%)`, '--border-hover': `hsl(${hue},${borderHoverS}%,${borderHoverL}%)`,
    '--shadow-color': `rgba(0,0,0,${shadowA})`,
    '--amber': `hsl(38,${mix(78,72)}%,${mix(57,42)}%)`,
    '--red': `hsl(0,${mix(68,62)}%,${mix(56,40)}%)`,
    '--purple': `hsl(256,${mix(82,60)}%,${mix(74,48)}%)`,
  };
}

function applyTheme(theme, hue, options = {}) {
  const { silent = false, vibrancy, lightness } = options;
  const el = document.documentElement;
  const clearVars = () => { Object.keys(generateThemeFromHue(0, 100, 0)).forEach(k => el.style.removeProperty(k)); };
  const lt = lightness ?? local.lightness;
  clearVars();

  let accentHue = PRESET_HUES[theme] ?? local.customHue;
  if (theme === 'custom' || lt > 0) {
    const h = theme === 'custom' ? (hue ?? local.customHue) : (PRESET_HUES[theme] || 140);
    const v = vibrancy ?? local.customVibrancy;
    accentHue = h;
    el.setAttribute('data-theme', 'custom');
    Object.entries(generateThemeFromHue(h, v, lt)).forEach(([k, val]) => el.style.setProperty(k, val));
    if (theme === 'custom') { local.customHue = h; local.customVibrancy = v; }
  } else {
    el.setAttribute('data-theme', theme);
  }

  el.setAttribute('data-color-mode', lt >= 50 ? 'light' : 'dark');
  local.lightness = lt;
  local.theme = theme;
  saveLocalState();

  // Logo hue shift
  el.style.setProperty('--logo-hue-shift', `${accentHue - LOGO_BASE_HUE}deg`);

  // Sync lightness sliders
  document.querySelectorAll('.lightness-slider').forEach(s => { s.value = lt; });

  dom.themePicker?.querySelectorAll('.theme-swatch').forEach(s => s.classList.toggle('active', s.dataset.theme === local.theme));

  // Animate color field marker to preset position
  if (theme !== 'custom' && PRESET_HUES[theme] != null) {
    animateMarkerToHue(dom.colorField, dom.colorFieldMarker, PRESET_HUES[theme]);
    animateMarkerToHue(dom.obColorField, dom.obColorFieldMarker, PRESET_HUES[theme]);
  }

  if (!silent) Sound.ui('theme');
}

function animateMarkerToHue(field, marker, hue) {
  if (!field || !marker) return;
  marker.classList.add('animating');
  marker.style.left = `${(hue / 360) * 100}%`;
  marker.style.top = '0%';
  marker.style.background = `hsl(${hue},65%,55%)`;
  setTimeout(() => marker.classList.remove('animating'), 380);
}

// ── Shortcut UI ──

function syncShortcutHints() {
  document.querySelectorAll('[data-action]').forEach(el => {
    const action = el.dataset.action;
    const label = Shortcuts.format(action);
    if (label) el.setAttribute('data-shortcut-hint', label);
    else el.removeAttribute('data-shortcut-hint');
  });
}

function renderShortcutEditor() {
  if (!dom.shortcutList) return;
  const labels = { settings: 'Settings', search: 'Search', addVersion: 'Add Version', launch: 'Launch', save: 'Save', close: 'Close' };
  dom.shortcutList.innerHTML = Shortcuts.all().map(action => {
    const b = Shortcuts.get(action);
    const isCustom = !!local.shortcuts[action];
    return `<div class="shortcut-item" data-sc-action="${action}">
      <span class="shortcut-key shortcut-item-key" data-sc-record="${action}" title="Click to change">${esc(Shortcuts.format(action))}</span>
      <span class="shortcut-desc">${esc(b.desc)}${isCustom ? ` <button class="shortcut-item-reset" data-sc-reset="${action}">reset</button>` : ''}</span>
    </div>`;
  }).join('');
}

let recordingAction = null;
function startRecording(action) {
  stopRecording();
  recordingAction = action;
  const el = dom.shortcutList?.querySelector(`[data-sc-record="${action}"]`);
  if (el) { el.classList.add('recording'); el.textContent = 'Press keys...'; }
}
function stopRecording() {
  if (!recordingAction) return;
  const el = dom.shortcutList?.querySelector(`[data-sc-record="${recordingAction}"]`);
  if (el) { el.classList.remove('recording'); el.textContent = Shortcuts.format(recordingAction); }
  recordingAction = null;
}
function handleRecordKey(e) {
  if (!recordingAction) return false;
  e.preventDefault(); e.stopPropagation();
  if (e.key === 'Escape') { stopRecording(); return true; }
  if (['Control', 'Shift', 'Alt', 'Meta'].includes(e.key)) return true;
  Shortcuts.set(recordingAction, { key: e.key, ctrl: e.ctrlKey, shift: e.shiftKey, alt: e.altKey, desc: Shortcuts.get(recordingAction).desc });
  local.shortcuts = Shortcuts._custom;
  saveLocalState();
  stopRecording();
  renderShortcutEditor();
  syncShortcutHints();
  Sound.ui('affirm');
  return true;
}

function positionFieldMarker(field, marker, hue, vibrancy) {
  if (!field || !marker) return;
  marker.style.left = `${(hue / 360) * 100}%`;
  marker.style.top = `${(1 - vibrancy / 100) * 100}%`;
  marker.style.background = `hsl(${hue},65%,55%)`;
}

function initColorField(field, marker, onDrag, onEnd) {
  if (!field) return;
  let active = false;
  function calc(e) {
    const r = field.getBoundingClientRect();
    const x = Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
    const y = Math.max(0, Math.min(1, (e.clientY - r.top) / r.height));
    return { hue: Math.round(x * 360), vibrancy: Math.round((1 - y) * 100) };
  }
  field.addEventListener('pointerdown', (e) => {
    active = true;
    field.setPointerCapture(e.pointerId);
    const c = calc(e);
    positionFieldMarker(field, marker, c.hue, c.vibrancy);
    onDrag(c.hue, c.vibrancy);
  });
  field.addEventListener('pointermove', (e) => {
    if (!active) return;
    const c = calc(e);
    positionFieldMarker(field, marker, c.hue, c.vibrancy);
    onDrag(c.hue, c.vibrancy);
  });
  field.addEventListener('pointerup', () => { active = false; if (onEnd) onEnd(); });
  field.addEventListener('lostpointercapture', () => { active = false; });
}

// ── App State ──

const state = {
  versions: [], config: null, systemInfo: null, devMode: false,
  selectedVersion: null, activeSession: null,
  eventSource: null, installEventSource: null,
  logLines: 0, filter: 'all', catalogFilter: 'release',
  search: '', catalogSearch: '', catalog: null,
  gameRunning: false, runningVersionId: null,
  installing: false, launching: false,
  currentPage: 'launcher',
};

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => document.querySelectorAll(sel);
const dom = {};
let lastMemorySoundAt = 0;
let lastHueSoundAt = 0;

function cacheDom() {
  const ids = [
    'version-list', 'version-search', 'empty-state', 'empty-title', 'empty-sub', 'empty-add-btn',
    'center-panel', 'page-stack', 'launcher-view', 'settings-view', 'settings-content', 'settings-nav', 'sidebar-launcher-panel', 'sidebar-settings-panel',
    'version-detail', 'detail-id', 'detail-badge', 'detail-props',
    'launch-area', 'launch-btn', 'launching-area', 'launch-ascii', 'launch-seq-version',
    'running-area', 'running-ascii', 'running-version', 'running-pid', 'running-uptime', 'kill-btn',
    'not-launchable', 'not-launchable-text',
    'install-area', 'install-text', 'install-btn', 'install-progress', 'progress-fill', 'progress-text',
    'username-input', 'memory-slider', 'memory-value', 'memory-rec',
    'log-panel', 'log-toggle', 'log-content', 'log-lines', 'log-count',
    'settings-btn', 'settings-cancel', 'settings-save',
    'setting-java-path', 'setting-width', 'setting-height', 'java-runtimes',
    'theme-picker', 'color-field', 'color-field-marker', 'lightness-slider', 'sounds-toggle', 'shortcut-list',
    'add-version-btn', 'catalog-modal', 'catalog-close', 'catalog-search', 'catalog-list',
    'onboarding', 'onboarding-step-1', 'onboarding-step-2', 'onboarding-step-3', 'onboarding-step-4',
    'onboarding-username', 'onboarding-ram-info', 'onboarding-memory-slider', 'onboarding-memory-value', 'onboarding-rec',
    'onboarding-next-1', 'onboarding-next-2', 'onboarding-next-3', 'onboarding-finish',
    'dot-1', 'dot-2', 'dot-3', 'dot-4',
    'ob-theme-presets', 'ob-color-field', 'ob-color-field-marker', 'ob-lightness-slider',
    'dev-tools', 'dev-cleanup', 'dev-flush',
  ];
  ids.forEach(id => { dom[id.replace(/-([a-z0-9])/g, (_, c) => c.toUpperCase())] = document.getElementById(id); });
}

// ── API ──

async function api(method, path, body) {
  const opts = { method, headers: { 'Content-Type': 'application/json' } };
  if (body) opts.body = JSON.stringify(body);
  return (await fetch(`${API}${path}`, opts)).json();
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
  dom.memoryRec.textContent = val < 2 ? '(low — may lag)' : val > totalGB * 0.75 ? '(high — leave room for OS)' : '';
}

// ── Pages ──

function setPage(page) {
  state.currentPage = page;
  dom.launcherView?.classList.toggle('hidden', page !== 'launcher');
  dom.settingsView?.classList.toggle('hidden', page !== 'settings');
  dom.sidebarLauncherPanel?.classList.toggle('hidden', page !== 'launcher');
  dom.sidebarSettingsPanel?.classList.toggle('hidden', page !== 'settings');
  dom.settingsBtn?.classList.toggle('active', page === 'settings');
}

function toggleShortcutHints(show) {
  document.body.classList.toggle('show-shortcuts', show);
}

// ══════════════════════════════════════════
// VERSION SELECTION
// ══════════════════════════════════════════

function selectVersion(version, options = {}) {
  const { silent = false } = options;
  if (!silent) { Sound.init(); Sound.tick(); }
  state.selectedVersion = version;
  setPage('launcher');
  renderSelectedVersion();
  renderVersionList();
}

function renderSelectedVersion() {
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

  scrambleText(dom.detailId, version.id, 300);

  const isModded = !!version.inherits_from;
  const badgeClass = isModded ? 'badge-modded' : version.type === 'release' ? 'badge-release' : version.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
  dom.detailBadge.className = `detail-badge ${badgeClass}`;
  dom.detailBadge.textContent = isModded ? 'Modded' : version.type === 'release' ? 'Release' : version.type === 'snapshot' ? 'Snapshot' : version.type || 'Unknown';

  if (dom.detailProps) dom.detailProps.innerHTML = buildVersionProps(version);
  refreshSelectedVersionActionState();
}

function buildVersionProps(version) {
  let props = '';
  if (version.java_component) props += prop('Runtime', version.java_component, true);
  if (version.java_major) props += prop('Java', `Java ${version.java_major}`);
  if (version.inherits_from) props += prop('Base', version.inherits_from);
  if (version.release_time) {
    const d = new Date(version.release_time);
    if (!isNaN(d)) props += prop('Released', d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' }));
  }
  const lastLaunched = formatLastLaunched(version.id);
  props += prop('Last launched', lastLaunched.text, lastLaunched.accent);
  if (version.status) props += prop('Status', version.launchable ? 'Ready' : version.status_detail || 'Incomplete', version.launchable);
  return props;
}

function formatLastLaunched(versionId) {
  const ts = state.config?.last_launched?.[versionId];
  if (!ts) return { text: 'Never', accent: false };
  const d = new Date(ts);
  if (isNaN(d)) return { text: ts, accent: true };
  return { text: new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(d), accent: true };
}

function prop(label, value, accent) {
  return `<div class="detail-prop"><span class="detail-prop-label">${label}</span><span class="detail-prop-value${accent ? ' accent' : ''}">${esc(String(value))}</span></div>`;
}

function hideAllActions() {
  [dom.launchArea, dom.installArea, dom.launchingArea, dom.runningArea, dom.notLaunchable].forEach(el => { if (el) el.classList.add('action-hidden'); });
  resetInstallUI();
}

function show(el) { if (el) el.classList.remove('action-hidden'); }

function showNotLaunchable(message) {
  if (dom.notLaunchableText) dom.notLaunchableText.textContent = message;
  show(dom.notLaunchable);
}

function refreshSelectedVersionActionState() {
  if (!state.selectedVersion) return;
  hideAllActions();
  const version = state.selectedVersion;

  if (state.launching) {
    if (state.runningVersionId === version.id) show(dom.launchingArea);
    else showNotLaunchable('Another launch is already being prepared.');
    return;
  }

  if (state.gameRunning) {
    if (state.runningVersionId === version.id) show(dom.runningArea);
    else showNotLaunchable(`${state.runningVersionId} is already running.`);
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

// ══════════════════════════════════════════
// SIDEBAR
// ══════════════════════════════════════════

function renderVersionList() {
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
      const isRunning = state.gameRunning && state.runningVersionId === v.id;
      const dc = isRunning ? 'running' : v.launchable ? 'ok' : 'missing';
      const sel = state.selectedVersion?.id === v.id ? 'selected' : '';
      const rc = isRunning ? 'is-running' : '';
      const dim = v.launchable ? '' : 'dimmed';
      html += `<button type="button" class="version-item ${dim} ${sel} ${rc}" data-id="${v.id}" aria-pressed="${sel ? 'true' : 'false'}" aria-label="Select version ${esc(v.id)}" style="animation-delay:${i * 15}ms"><div class="version-dot ${dc}"></div><span class="version-name">${esc(v.id)}</span>${isRunning ? '<span class="version-running-tag">LIVE</span>' : ''}<span class="version-badge ${bc}">${bt}</span></button>`;
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
    el.addEventListener('click', () => {
      if (v) selectVersion(v);
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

function filterVersions(versions) {
  let list = versions;
  if (state.filter === 'release') list = list.filter(v => v.type === 'release' && !v.inherits_from);
  else if (state.filter === 'snapshot') list = list.filter(v => v.type === 'snapshot' && !v.inherits_from);
  else if (state.filter === 'modded') list = list.filter(v => !!v.inherits_from);
  if (state.search) { const q = state.search.toLowerCase(); list = list.filter(v => v.id.toLowerCase().includes(q)); }
  return list;
}

// ══════════════════════════════════════════
// INSTALL
// ══════════════════════════════════════════

async function installVersion() {
  if (!state.selectedVersion || state.installing) return;
  state.installing = true;
  const target = dom.installBtn?.dataset.installTarget || state.selectedVersion.id;
  if (dom.installBtn) {
    dom.installBtn.disabled = true;
    const label = dom.installBtn.querySelector('.install-btn-text');
    if (label) label.textContent = 'INSTALLING...';
  }
  show(dom.installProgress);
  if (dom.progressText) dom.progressText.textContent = target !== state.selectedVersion.id ? `Installing base version ${target}...` : 'Starting download...';
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
    const btn = dom.catalogList?.querySelector(`[data-install-id="${versionId}"]`);
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
    if (dom.progressFill) dom.progressFill.style.width = pct + '%';
    if (dom.progressText) {
      dom.progressText.textContent = d.phase === 'done' ? 'Complete!' : d.phase === 'libraries' ? `Libraries (${d.current}/${d.total})` : d.phase === 'client_jar' ? 'Downloading game...' : d.phase === 'version_json' ? 'Fetching version info...' : d.phase;
    }
    if (d.done) onInstallDone(catalogVersionId);
  });
  es.onerror = () => { if (state.installing) onInstallDone(catalogVersionId); };
}

async function onInstallDone(catalogVersionId) {
  state.installing = false;
  if (state.installEventSource) { state.installEventSource.close(); state.installEventSource = null; }
  if (dom.progressFill) dom.progressFill.style.width = '100%';
  if (dom.progressText) dom.progressText.textContent = 'Complete!';
  try {
    const res = await api('GET', '/versions');
    state.versions = res.versions || [];
    renderVersionList();
    if (catalogVersionId) {
      const btn = dom.catalogList?.querySelector(`[data-install-id="${catalogVersionId}"]`);
      if (btn) btn.outerHTML = `<span class="catalog-installed-badge">Installed</span>`;
    }
    if (state.selectedVersion) {
      const updated = state.versions.find(v => v.id === state.selectedVersion.id);
      if (updated) {
        state.selectedVersion = updated;
        renderSelectedVersion();
      }
    }
  } catch {
    resetInstallUI();
  }
}

function resetInstallUI() {
  state.installing = false;
  if (dom.installBtn) {
    dom.installBtn.disabled = false;
    const t = dom.installBtn.querySelector('.install-btn-text');
    if (t) t.textContent = 'INSTALL';
  }
  dom.installProgress?.classList.add('action-hidden');
  if (dom.progressFill) dom.progressFill.style.width = '0%';
}

// ══════════════════════════════════════════
// CATALOG
// ══════════════════════════════════════════

async function openCatalog() {
  restoreFocusEl = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  dom.catalogModal?.classList.remove('hidden');
  if (dom.catalogSearch) dom.catalogSearch.value = '';
  state.catalogSearch = '';
  if (dom.catalogList) dom.catalogList.innerHTML = `<div class="loading-placeholder"><div class="spinner"></div><span>Loading...</span></div>`;
  setTimeout(() => dom.catalogSearch?.focus(), 0);
  try {
    state.catalog = await api('GET', '/catalog');
    renderCatalog();
  } catch {
    if (dom.catalogList) dom.catalogList.innerHTML = `<div class="loading-placeholder"><span style="color:var(--red)">Failed to load</span></div>`;
  }
}

function closeCatalog() {
  dom.catalogModal?.classList.add('hidden');
  restoreFocusEl?.focus?.();
}

function renderCatalog() {
  if (!state.catalog?.versions || !dom.catalogList) return;
  let list = state.catalog.versions.filter(v => v.type === state.catalogFilter);
  if (state.catalogSearch) { const q = state.catalogSearch.toLowerCase(); list = list.filter(v => v.id.toLowerCase().includes(q)); }
  const display = list.slice(0, 50);
  if (!display.length) {
    dom.catalogList.innerHTML = `<div class="loading-placeholder"><span>No versions found</span></div>`;
    return;
  }
  dom.catalogList.innerHTML = display.map(v => {
    const bc = v.type === 'release' ? 'badge-release' : v.type === 'snapshot' ? 'badge-snapshot' : 'badge-old';
    const bt = v.type === 'release' ? 'REL' : v.type === 'snapshot' ? 'SNAP' : v.type.toUpperCase().slice(0, 4);
    const ds = new Date(v.release_time);
    const dateStr = !isNaN(ds) ? ds.toLocaleDateString() : '';
    const act = v.installed ? `<span class="catalog-installed-badge">Installed</span>` : `<button class="catalog-install-btn" data-install-id="${esc(v.id)}" data-url="${esc(v.url)}">Install</button>`;
    return `<div class="catalog-item"><div class="catalog-item-info"><span class="catalog-item-id">${esc(v.id)}</span><span class="catalog-item-date">${dateStr}</span></div><span class="version-badge ${bc}">${bt}</span>${act}</div>`;
  }).join('') + (list.length > 50 ? `<div class="loading-placeholder"><span style="font-size:10px;color:var(--text-muted)">Showing 50 of ${list.length}</span></div>` : '');
  dom.catalogList.querySelectorAll('.catalog-install-btn').forEach(btn => btn.addEventListener('click', () => installFromCatalog(btn.dataset.installId, btn.dataset.url)));
}

// ══════════════════════════════════════════
// LAUNCH
// ══════════════════════════════════════════

let launchSeqInterval = null;
let runningAnimInterval = null;
let uptimeInterval = null;
let uptimeStart = 0;
let restoreFocusEl = null;

function clearLaunchVisualState() {
  endLaunchSequence();
  stopRunningAnimation();
  stopUptime();
  if (dom.runningUptime) dom.runningUptime.textContent = '0:00';
  if (dom.runningPid) dom.runningPid.textContent = '';
  if (dom.runningVersion) dom.runningVersion.textContent = '';
}

async function launchGame() {
  if (!state.selectedVersion || state.gameRunning || state.launching) return;
  Sound.init();

  const versionId = state.selectedVersion.id;
  const username = dom.usernameInput?.value.trim() || 'Player';
  const maxMemMB = Math.round(parseFloat(dom.memorySlider?.value || 4) * 1024);

  clearLaunchVisualState();
  state.launching = true;
  state.runningVersionId = versionId;
  state.activeSession = null;
  if (dom.launchSeqVersion) dom.launchSeqVersion.textContent = versionId;
  refreshSelectedVersionActionState();
  startLaunchSequence();
  renderVersionList();

  try {
    const res = await api('POST', '/launch', { version_id: versionId, username, max_memory_mb: maxMemMB });
    if (res.error) {
      showError(res.error);
      clearLaunchVisualState();
      state.launching = false;
      state.runningVersionId = null;
      refreshSelectedVersionActionState();
      renderVersionList();
      return;
    }

    state.activeSession = res.session_id;
    state.launching = false;
    state.gameRunning = true;
    state.runningVersionId = versionId;

    endLaunchSequence();
    Sound.ui('launchSuccess');
    if (dom.runningVersion) dom.runningVersion.textContent = versionId;
    if (dom.runningPid) dom.runningPid.textContent = `PID ${res.pid}`;
    startRunningAnimation();
    startUptime();
    refreshSelectedVersionActionState();
    renderVersionList();
    dom.logPanel?.classList.add('expanded');
    connectLaunchSSE(res.session_id);
    markVersionLaunched(versionId, res.launched_at || new Date().toISOString(), username, maxMemMB);
  } catch (err) {
    showError(err.message);
    clearLaunchVisualState();
    state.launching = false;
    state.runningVersionId = null;
    refreshSelectedVersionActionState();
    renderVersionList();
  }
}

function markVersionLaunched(versionId, launchedAt, username, maxMemMB) {
  if (!state.config) state.config = {};
  state.config.username = username;
  state.config.max_memory_mb = maxMemMB;
  state.config.last_version_id = versionId;
  state.config.last_launched = { ...(state.config.last_launched || {}), [versionId]: launchedAt };
  if (state.selectedVersion?.id === versionId && dom.detailProps) dom.detailProps.innerHTML = buildVersionProps(state.selectedVersion);
}

function connectLaunchSSE(sessionId) {
  if (state.eventSource) state.eventSource.close();
  const es = new EventSource(`${API}/launch/${sessionId}/events`);
  state.eventSource = es;
  es.addEventListener('status', (e) => {
    if (state.activeSession !== sessionId) return;
    const d = JSON.parse(e.data);
    if (d.state === 'exited') onGameExited(d.exit_code, sessionId);
  });
  es.addEventListener('log', (e) => {
    if (state.activeSession !== sessionId) return;
    const d = JSON.parse(e.data);
    appendLog(d.source, d.text);
  });
  es.onerror = () => {
    if (state.activeSession === sessionId && state.gameRunning) onGameExited(-1, sessionId);
  };
}

function onGameExited(exitCode, sessionId) {
  if (sessionId && state.activeSession && sessionId !== state.activeSession) return;
  state.gameRunning = false;
  state.launching = false;
  state.runningVersionId = null;
  state.activeSession = null;
  if (state.eventSource) { state.eventSource.close(); state.eventSource = null; }
  clearLaunchVisualState();
  refreshSelectedVersionActionState();
  appendLog('system', `Game exited with code ${exitCode}`);
  renderVersionList();
}

async function killGame() {
  if (!state.activeSession) return;
  try {
    await api('POST', `/launch/${state.activeSession}/kill`);
  } catch (err) {
    showError('Failed to kill: ' + err.message);
  }
}

// ── Log ──

function appendLog(source, text) {
  const line = document.createElement('div');
  line.className = `log-line ${source}`;
  line.textContent = text;
  dom.logLines?.appendChild(line);
  state.logLines++;
  if (dom.logCount) dom.logCount.textContent = `${state.logLines} lines`;
  if (dom.logContent) dom.logContent.scrollTop = dom.logContent.scrollHeight;
}

// ── Settings ──

function openSettings() {
  restoreFocusEl = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  syncSettingsForm();
  setPage('settings');
  if (dom.settingsContent) dom.settingsContent.scrollTop = 0;
  syncSettingsSectionNav();
  loadJavaRuntimes();
  setTimeout(() => dom.settingsNav?.querySelector('.settings-nav-btn.active')?.focus(), 0);
}

function closeSettings() {
  setPage('launcher');
  renderSelectedVersion();
  restoreFocusEl?.focus?.();
}

function syncSettingsForm() {
  if (state.config) {
    if (dom.settingJavaPath) dom.settingJavaPath.value = state.config.java_path_override || '';
    if (dom.settingWidth) dom.settingWidth.value = state.config.window_width || '';
    if (dom.settingHeight) dom.settingHeight.value = state.config.window_height || '';
  }
  dom.themePicker?.querySelectorAll('.theme-swatch').forEach(s => s.classList.toggle('active', s.dataset.theme === local.theme));
  positionFieldMarker(dom.colorField, dom.colorFieldMarker, local.customHue, local.customVibrancy);
  if (dom.soundsToggle) dom.soundsToggle.checked = Sound.enabled;
  renderShortcutEditor();
}

async function saveSettings() {
  const updates = {};
  const jp = dom.settingJavaPath?.value.trim() || '';
  if (jp !== (state.config?.java_path_override || '')) updates.java_path_override = jp;

  const widthRaw = dom.settingWidth?.value.trim() || '';
  const heightRaw = dom.settingHeight?.value.trim() || '';
  const w = widthRaw === '' ? 0 : parseInt(widthRaw, 10) || 0;
  const h = heightRaw === '' ? 0 : parseInt(heightRaw, 10) || 0;
  if (w !== (state.config?.window_width || 0)) updates.window_width = w;
  if (h !== (state.config?.window_height || 0)) updates.window_height = h;

  if (Object.keys(updates).length) {
    const r = await api('PUT', '/config', updates);
    if (!r.error) state.config = r;
  }
  closeSettings();
}

function syncSettingsSectionNav() {
  if (!dom.settingsContent || !dom.settingsNav) return;
  const sections = [...dom.settingsContent.querySelectorAll('.settings-section-card')].filter(section => !section.classList.contains('hidden'));
  if (!sections.length) return;
  const contentTop = dom.settingsContent.getBoundingClientRect().top;
  let activeId = sections[0].id;
  let best = Number.POSITIVE_INFINITY;
  sections.forEach(section => {
    const distance = Math.abs(section.getBoundingClientRect().top - contentTop - 18);
    if (distance < best) {
      best = distance;
      activeId = section.id;
    }
  });
  dom.settingsNav.querySelectorAll('.settings-nav-btn').forEach(btn => btn.classList.toggle('active', btn.dataset.settingsTarget === activeId));
}

async function loadJavaRuntimes() {
  if (!dom.javaRuntimes) return;
  try {
    const res = await api('GET', '/java');
    const rt = res.runtimes || [];
    dom.javaRuntimes.innerHTML = rt.length === 0 ? '<span class="setting-hint">No runtimes detected</span>' :
      rt.map(r => `<div class="java-runtime-item"><span class="java-runtime-component">${esc(r.Component || r.component)}</span><span class="java-runtime-source">${esc(r.Source || r.source)}</span></div>`).join('');
  } catch {
    dom.javaRuntimes.innerHTML = '<span class="setting-hint">Failed to load</span>';
  }
}

// ── Onboarding ──

function showOnboarding() {
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

function onboardingStep(n) {
  [dom.onboardingStep1, dom.onboardingStep2, dom.onboardingStep3, dom.onboardingStep4].forEach((s, i) => { if (s) s.classList.toggle('hidden', i !== n - 1); });
  [dom.dot1, dom.dot2, dom.dot3, dom.dot4].forEach((d, i) => { if (d) d.classList.toggle('active', i === n - 1); });
}

async function finishOnboarding() {
  const username = dom.onboardingUsername?.value.trim() || 'Player';
  const memGB = parseFloat(dom.onboardingMemorySlider?.value || 4);
  if (dom.usernameInput) dom.usernameInput.value = username;
  if (dom.memorySlider) {
    dom.memorySlider.value = memGB;
    if (dom.memoryValue) dom.memoryValue.textContent = fmtMem(memGB);
  }
  try {
    const r = await api('PUT', '/config', { username, max_memory_mb: Math.round(memGB * 1024) });
    if (!r.error) state.config = r;
  } catch {}
  try { await api('POST', '/onboarding/complete'); } catch {}
  dom.onboarding?.classList.add('hidden');
}


// ── Animations ──

const LAUNCH_FRAMES = [
  ['   ╭──────────────╮   ', '   │  ▓▓▓▓▓▓▓▓▓▓  │   ', '   │ ▓▓  ◉  ◉  ▓▓ │   ', '   │ ▓▓   ▔▔   ▓▓ │   ', '   │ ▓▓  ╲__/  ▓▓ │   ', '   │  ▓▓▓▓▓▓▓▓▓▓  │   ', '   ╰──────────────╯   '],
  ['   ╭──────────────╮   ', '   │  ░▓▓▓▓▓▓▓▓░  │   ', '   │ ▓░  ◌  ◌  ░▓ │   ', '   │ ▓░   ▔▔   ░▓ │   ', '   │ ▓░  ╲__/  ░▓ │   ', '   │  ░▓▓▓▓▓▓▓▓░  │   ', '   ╰──────────────╯   '],
  ['   ╔══════════════╗   ', '   ║  ██████████  ║   ', '   ║ ██  ◆  ◆  ██ ║   ', '   ║ ██   ▔▔   ██ ║   ', '   ║ ██  ╱__╲  ██ ║   ', '   ║  ██████████  ║   ', '   ╚══════════════╝   '],
  ['   ╔══════════════╗   ', '   ║  ███▓▓▓▓███  ║   ', '   ║ ██  ◈  ◈  ██ ║   ', '   ║ ██   ▂▂   ██ ║   ', '   ║ ██  ╲──╱  ██ ║   ', '   ║  ███▓▓▓▓███  ║   ', '   ╚══════════════╝   '],
];

function startLaunchSequence() {
  endLaunchSequence();
  let f = 0;
  if (dom.launchAscii) dom.launchAscii.textContent = LAUNCH_FRAMES[0].join('\n');
  launchSeqInterval = setInterval(() => {
    f = (f + 1) % LAUNCH_FRAMES.length;
    if (dom.launchAscii) dom.launchAscii.textContent = LAUNCH_FRAMES[f].join('\n');
  }, 320);
}

function endLaunchSequence() {
  if (launchSeqInterval) {
    clearInterval(launchSeqInterval);
    launchSeqInterval = null;
  }
}

const RUNNING_FRAMES = [
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( -.- )', ' > ^ < '],
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( ^.^ )', ' > ^ < '],
  [' /\\_/\\ ', '( ^.^ )', ' > ^ < '],
  [' /\\_/\\ ', '( o.o )', ' > ^ < '],
  [' /\\_/\\ ', '( -.o )', ' > ^ < '],
];

function startRunningAnimation() {
  stopRunningAnimation();
  if (!dom.runningAscii) return;
  let f = 0;
  dom.runningAscii.textContent = RUNNING_FRAMES[0].join('\n');
  runningAnimInterval = setInterval(() => {
    f = (f + 1) % RUNNING_FRAMES.length;
    dom.runningAscii.textContent = RUNNING_FRAMES[f].join('\n');
  }, 900);
}

function stopRunningAnimation() {
  if (runningAnimInterval) {
    clearInterval(runningAnimInterval);
    runningAnimInterval = null;
  }
}

function startUptime() {
  stopUptime();
  uptimeStart = Date.now();
  if (dom.runningUptime) dom.runningUptime.textContent = '0:00';
  uptimeInterval = setInterval(() => {
    const elapsed = Math.floor((Date.now() - uptimeStart) / 1000);
    if (dom.runningUptime) dom.runningUptime.textContent = `${Math.floor(elapsed / 60)}:${(elapsed % 60).toString().padStart(2, '0')}`;
  }, 1000);
}

function stopUptime() {
  if (uptimeInterval) {
    clearInterval(uptimeInterval);
    uptimeInterval = null;
  }
}

// ── Utilities ──

function showError(msg) {
  appendLog('stderr', `ERROR: ${msg}`);
  dom.logPanel?.classList.add('expanded');
}

function esc(s) {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}

function fmtMem(gb) { return gb === Math.floor(gb) ? `${gb}\u00A0GB` : `${gb.toFixed(1)}\u00A0GB`; }

function inferButtonSound(btn) {
  if (btn.classList.contains('version-item') || btn.classList.contains('theme-swatch') || btn.classList.contains('ob-theme-btn') || btn.classList.contains('settings-nav-btn')) return null;
  if (btn.classList.contains('chip')) return 'soft';
  if (btn.id === 'launch-btn') return 'launchPress';
  if (btn.id === 'add-version-btn' || btn.id === 'empty-add-btn') return 'bright';
  if (btn.id === 'settings-save' || btn.id === 'install-btn' || btn.classList.contains('catalog-install-btn') || btn.id === 'onboarding-finish') return 'affirm';
  if (btn.id === 'settings-cancel' || btn.id === 'catalog-close' || btn.id === 'kill-btn') return 'soft';
  return 'click';
}

function bindButtonSounds() {
  document.addEventListener('click', (e) => {
    const btn = e.target.closest('button');
    if (!btn || btn.disabled) return;
    const kind = inferButtonSound(btn);
    if (kind) Sound.ui(kind);
  });
}

function playSliderSound(value, family) {
  const now = performance.now();
  const limit = family === 'memory' ? 55 : 45;
  const ref = family === 'memory' ? lastMemorySoundAt : lastHueSoundAt;
  if (now - ref < limit) return;
  if (family === 'memory') lastMemorySoundAt = now;
  else lastHueSoundAt = now;
  Sound.ui(family === 'memory' ? 'memory' : 'slider', Math.max(0, Math.min(1, value)));
}

// ══════════════════════════════════════════
// INIT & EVENT BINDINGS
// ══════════════════════════════════════════

async function init() {
  cacheDom();
  applyTheme(local.theme, local.customHue, { silent: true, vibrancy: local.customVibrancy, lightness: local.lightness });
  Sound.enabled = local.sounds;
  Sound.warmup();
  if (dom.soundsToggle) dom.soundsToggle.checked = local.sounds;
  if (local.logExpanded) dom.logPanel?.classList.add('expanded');
  state.filter = local.sidebarFilter;
  $$('.filter-chips .chip[data-filter]').forEach(c => c.classList.toggle('active', c.dataset.filter === state.filter));
  positionFieldMarker(dom.colorField, dom.colorFieldMarker, local.customHue, local.customVibrancy);
  syncShortcutHints();
  renderShortcutEditor();
  setPage('launcher');

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
    if (state.config?.last_version_id) {
      const remembered = state.versions.find(v => v.id === state.config.last_version_id);
      if (remembered) selectVersion(remembered, { silent: true });
    }
    if (state.config && !state.config.onboarding_done) showOnboarding();
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
  const activateSound = () => Sound.activate();
  window.addEventListener('pointerdown', activateSound, { once: true, capture: true });
  window.addEventListener('touchstart', activateSound, { once: true, capture: true });
  window.addEventListener('keydown', activateSound, { once: true, capture: true });

  dom.versionSearch?.addEventListener('input', (e) => {
    state.search = e.target.value;
    renderVersionList();
  });

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

  dom.logToggle?.addEventListener('click', () => {
    dom.logPanel?.classList.toggle('expanded');
    local.logExpanded = dom.logPanel?.classList.contains('expanded');
    saveLocalState();
  });

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

  dom.addVersionBtn?.addEventListener('click', openCatalog);
  dom.emptyAddBtn?.addEventListener('click', openCatalog);
  dom.catalogClose?.addEventListener('click', closeCatalog);
  dom.catalogModal?.addEventListener('click', (e) => { if (e.target === dom.catalogModal) closeCatalog(); });
  dom.catalogSearch?.addEventListener('input', (e) => {
    state.catalogSearch = e.target.value;
    renderCatalog();
  });
  $$('.chip[data-catalog-filter]').forEach(chip => {
    chip.addEventListener('click', () => {
      chip.parentElement.querySelectorAll('.chip').forEach(c => c.classList.remove('active'));
      chip.classList.add('active');
      state.catalogFilter = chip.dataset.catalogFilter;
      renderCatalog();
    });
  });

  dom.onboardingNext1?.addEventListener('click', () => onboardingStep(2));
  dom.onboardingNext2?.addEventListener('click', () => onboardingStep(3));
  dom.onboardingNext3?.addEventListener('click', () => onboardingStep(4));
  dom.onboardingFinish?.addEventListener('click', finishOnboarding);
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
    if (!confirm('Remove all installed versions?\nWorlds/mods will be backed up.')) return;
    dom.devCleanup.disabled = true;
    dom.devCleanup.textContent = 'Working...';
    try {
      const res = await api('POST', '/dev/cleanup-versions');
      if (res.error) showError(res.error);
      else {
        appendLog('system', `Removed ${res.removed} versions`);
        state.versions = (await api('GET', '/versions')).versions || [];
        state.selectedVersion = null;
        dom.versionDetail?.classList.add('hidden');
        dom.emptyState?.classList.remove('hidden');
        renderVersionList();
      }
    } catch (err) {
      showError(err.message);
    }
    dom.devCleanup.disabled = false;
    dom.devCleanup.textContent = 'Cleanup Versions';
  });

  dom.devFlush?.addEventListener('click', async () => {
    if (!confirm('Delete all settings? App will restart.')) return;
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
    if (Shortcuts.matches(e, 'addVersion')) {
      e.preventDefault();
      if (state.currentPage === 'settings') closeSettings();
      setPage('launcher');
      Sound.ui('bright');
      openCatalog();
      return;
    }
    if (Shortcuts.matches(e, 'launch')) {
      e.preventDefault();
      if (state.currentPage === 'settings') closeSettings();
      setPage('launcher');
      if (state.selectedVersion?.launchable && !state.launching && !state.gameRunning) { Sound.ui('launchPress'); launchGame(); }
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
      Sound.ui('affirm');
      saveSettings();
      return;
    }
    if (Shortcuts.matches(e, 'close')) {
      if (!dom.catalogModal?.classList.contains('hidden')) closeCatalog();
      else if (state.currentPage === 'settings') closeSettings();
    }
    if (e.key === 'Enter' && dom.onboarding && !dom.onboarding.classList.contains('hidden')) {
      e.preventDefault();
      if (!dom.onboardingStep1?.classList.contains('hidden')) onboardingStep(2);
      else if (!dom.onboardingStep2?.classList.contains('hidden')) onboardingStep(3);
      else if (!dom.onboardingStep3?.classList.contains('hidden')) onboardingStep(4);
      else if (!dom.onboardingStep4?.classList.contains('hidden')) finishOnboarding();
    }
  });
  document.addEventListener('keyup', (e) => {
    if (e.key === 'Control') toggleShortcutHints(false);
  });
  window.addEventListener('blur', () => toggleShortcutHints(false));
}

init();
