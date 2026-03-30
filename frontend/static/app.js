// ═══════════════════════════════════════════
// Croopor — Frontend Application
// ═══════════════════════════════════════════

const API = '/api/v1';
const STORAGE_KEY = 'croopor_ui';

// ── Local UI State ──

const PRESET_HUES = { obsidian: 140, deepslate: 215, nether: 15, end: 268, birch: 100 };
const LOGO_BASE_HUE = 106;

const SHORTCUT_DEFAULTS = {
  settings:    { key: ',', ctrl: true, desc: 'Open or close settings' },
  search:      { key: 'f', ctrl: true, desc: 'Focus instance search' },
  newInstance: { key: 'n', ctrl: true, desc: 'New instance' },
  launch:      { key: 'Enter', ctrl: true, desc: 'Launch selected instance' },
  save:        { key: 's', ctrl: true, desc: 'Save settings' },
  close:       { key: 'Escape', ctrl: false, desc: 'Close dialogs' },
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
  // Set final text first to measure layout, then animate
  el.textContent = text;
  const finalHeight = el.offsetHeight;
  el.style.minHeight = finalHeight + 'px';
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
    if (step >= steps) { clearInterval(id); scrambleTimers.delete(el); el.textContent = text; el.style.minHeight = ''; }
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
  const labels = { settings: 'Settings', search: 'Search', newInstance: 'New Instance', launch: 'Launch', save: 'Save', close: 'Close' };
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
  instances: [], versions: [], config: null, systemInfo: null, devMode: false,
  selectedInstance: null, selectedVersion: null, activeSession: null,
  eventSource: null, installEventSource: null,
  logLines: 0, filter: 'all',
  search: '', catalog: null,
  gameRunning: false, runningInstanceId: null, runningVersionId: null,
  installing: false, launching: false,
  currentPage: 'launcher',
  lastInstanceId: null,
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
    'setting-java-path', 'setting-width', 'setting-height', 'java-runtimes', 'jvm-preset-group',
    'theme-picker', 'color-field', 'color-field-marker', 'lightness-slider', 'sounds-toggle', 'shortcut-list',
    'add-version-btn',
    'onboarding', 'onboarding-step-1', 'onboarding-step-2', 'onboarding-step-3', 'onboarding-step-4',
    'onboarding-username', 'onboarding-ram-info', 'onboarding-memory-slider', 'onboarding-memory-value', 'onboarding-rec',
    'onboarding-next-1', 'onboarding-next-2', 'onboarding-next-3', 'onboarding-finish',
    'dot-1', 'dot-2', 'dot-3', 'dot-4',
    'ob-theme-presets', 'ob-color-field', 'ob-color-field-marker', 'ob-lightness-slider',
    'dev-tools', 'dev-cleanup', 'dev-flush',
    'setup-overlay', 'setup-path-input', 'setup-path-error', 'setup-browse-btn', 'setup-use-btn',
    'setup-new-path', 'setup-init-btn',
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
// INSTANCE SELECTION
// ══════════════════════════════════════════

function selectInstance(inst, options = {}) {
  const { silent = false } = options;
  if (!silent) { Sound.init(); Sound.tick(); }
  state.selectedInstance = inst;
  // Also set selectedVersion for install flow compatibility
  state.selectedVersion = inst ? state.versions.find(v => v.id === inst.version_id) || null : null;
  setPage('launcher');
  renderSelectedInstance();
  // Update selection classes without re-rendering the full list (avoids killing click handlers)
  dom.versionList?.querySelectorAll('.version-item').forEach(el => {
    const selected = el.dataset.id === inst?.id;
    el.classList.toggle('selected', selected);
    el.setAttribute('aria-pressed', selected ? 'true' : 'false');
  });
}

// Keep for catalog/install compatibility
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
  dom.detailBadge.textContent = isModded ? 'MOD' : version.type === 'release' ? 'REL' : version.type === 'snapshot' ? 'SNAP' : version.type?.toUpperCase()?.slice(0, 4) || '?';

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

function renderSelectedInstance() {
  const inst = state.selectedInstance;
  if (!inst) {
    dom.versionDetail?.classList.add('hidden');
    dom.emptyState?.classList.remove('hidden');
    return;
  }
  dom.emptyState?.classList.add('hidden');
  dom.versionDetail?.classList.remove('hidden');

  // Instance name as the hero
  scrambleText(dom.detailId, inst.name, 300);

  // Version badge
  const version = state.versions.find(v => v.id === inst.version_id);
  const vType = inst.version_type || version?.type || '';
  const isModded = version?.inherits_from;
  const badgeClass = isModded ? 'badge-modded' : vType === 'release' ? 'badge-release' : vType === 'snapshot' ? 'badge-snapshot' : 'badge-old';
  dom.detailBadge.className = `detail-badge ${badgeClass}`;
  dom.detailBadge.textContent = isModded ? 'MOD' : vType === 'release' ? 'REL' : vType === 'snapshot' ? 'SNAP' : vType?.toUpperCase()?.slice(0, 4) || '?';

  // Metadata line replaces prop grid
  if (dom.detailProps) dom.detailProps.innerHTML = buildInstanceMeta(inst, version);

  // Quick links
  let linksEl = document.getElementById('instance-links');
  if (!linksEl) {
    linksEl = document.createElement('div');
    linksEl.id = 'instance-links';
    linksEl.className = 'instance-links';
    dom.detailProps?.parentNode?.insertBefore(linksEl, dom.detailProps.nextSibling);
  }
  linksEl.innerHTML = `<a class="instance-link" data-sub="saves">Open saves</a><a class="instance-link" data-sub="mods">Open mods</a><a class="instance-link" data-sub="resourcepacks">Open resources</a><a class="instance-link" data-sub="">Open folder</a>`;
  linksEl.querySelectorAll('.instance-link').forEach(a => {
    a.addEventListener('click', () => {
      const sub = a.dataset.sub;
      api('POST', `/instances/${encodeURIComponent(inst.id)}/open-folder${sub ? '?sub=' + sub : ''}`);
      Sound.ui('click');
    });
  });

  refreshSelectedInstanceActionState();
}

function buildInstanceMeta(inst, version) {
  const parts = [];
  parts.push(esc(inst.version_id));
  if (version?.java_major) parts.push(`Java ${version.java_major}`);
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

function formatRelativeTime(date) {
  const now = new Date();
  const diff = now - date;
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  if (days < 7) return `${days}d ago`;
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' }).format(date);
}

function refreshSelectedInstanceActionState() {
  const inst = state.selectedInstance;
  if (!inst) return;
  hideAllActions();

  if (state.launching) {
    if (state.runningInstanceId === inst.id) show(dom.launchingArea);
    else showNotLaunchable('Another launch is already being prepared.');
    return;
  }

  if (state.gameRunning) {
    if (state.runningInstanceId === inst.id) show(dom.runningArea);
    else showNotLaunchable('Another instance is already running.');
    return;
  }

  if (state.installing) {
    show(dom.installArea);
    return;
  }

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

function formatLastLaunched(versionId) {
  // Legacy — kept for version detail compatibility
  return { text: 'N/A', accent: false };
}

function prop(label, value, accent) {
  return `<div class="detail-prop"><span class="detail-prop-label">${label}</span><span class="detail-prop-value${accent ? ' accent' : ''}">${esc(String(value))}</span></div>`;
}

function hideAllActions() {
  [dom.launchArea, dom.launchingArea, dom.runningArea, dom.notLaunchable].forEach(el => { if (el) el.classList.add('action-hidden'); });
  // Only reset install UI if no install is actively running
  if (!state.installing) {
    dom.installArea?.classList.add('action-hidden');
    resetInstallUI();
  }
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

  // Active install: keep showing install area with progress
  if (state.installing) {
    show(dom.installArea);
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
// VERSION WATCHER (detect third-party installs)
// ══════════════════════════════════════════

function watchVersions() {
  if (state.versionWatcher) state.versionWatcher.close();
  const es = new EventSource(`${API}/versions/watch`);
  state.versionWatcher = es;
  es.addEventListener('versions_changed', (e) => {
    try {
      const d = JSON.parse(e.data);
      const newVersions = d.versions || [];
      state.versions = newVersions;
      renderInstanceList();
      // Update selected instance action state (version may have become launchable)
      if (state.selectedInstance) {
        renderSelectedInstance();
      }
    } catch {}
  });
  es.onerror = () => {
    // Reconnect after a delay if connection drops
    es.close();
    state.versionWatcher = null;
    setTimeout(watchVersions, 5000);
  };
}

// ══════════════════════════════════════════
// SIDEBAR — INSTANCE LIST
// ══════════════════════════════════════════

function renderInstanceList() {
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
      const isRunning = state.gameRunning && state.runningInstanceId === inst.id;
      const dc = isRunning ? 'running' : v?.launchable ? 'ok' : 'missing';
      const sel = state.selectedInstance?.id === inst.id ? 'selected' : '';
      const rc = isRunning ? 'is-running' : '';
      const dim = v?.launchable ? '' : 'dimmed';
      html += `<button type="button" class="version-item ${dim} ${sel} ${rc}" data-id="${inst.id}" aria-pressed="${sel ? 'true' : 'false'}" aria-label="Select instance ${esc(inst.name)}" style="animation-delay:${i * 15}ms"><div class="version-dot ${dc}"></div><span class="version-name">${esc(inst.name)}</span><span class="version-sub">${esc(inst.version_id)}</span>${isRunning ? '<span class="version-running-tag">LIVE</span>' : ''}<span class="version-badge ${bc}">${bt}</span></button>`;
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

function filterInstances(instances) {
  let list = instances;
  const versionMap = {};
  for (const v of state.versions) versionMap[v.id] = v;

  if (state.filter === 'release') list = list.filter(inst => { const v = versionMap[inst.version_id]; return v?.type === 'release' && !v?.inherits_from; });
  else if (state.filter === 'snapshot') list = list.filter(inst => { const v = versionMap[inst.version_id]; return v?.type === 'snapshot' && !v?.inherits_from; });
  else if (state.filter === 'modded') list = list.filter(inst => { const v = versionMap[inst.version_id]; return !!v?.inherits_from; });

  if (state.search) { const q = state.search.toLowerCase(); list = list.filter(inst => inst.name.toLowerCase().includes(q) || inst.version_id.toLowerCase().includes(q)); }
  return list;
}

// ══════════════════════════════════════════
// SIDEBAR — VERSION LIST (kept for catalog install flow)
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
    el.addEventListener('click', (e) => {
      if (e.button !== 0) return; // ignore right/middle clicks
      if (v) selectVersion(v);
    });
    el.addEventListener('contextmenu', (e) => {
      if (v) {
        e.preventDefault();
        e.stopPropagation(); // prevent document-level handler from immediately hiding the menu
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
  if (state.installing) return;
  const target = dom.installBtn?.dataset.installTarget || state.selectedVersion?.id || state.selectedInstance?.version_id;
  if (!target) return;
  state.installing = true;
  if (dom.installBtn) {
    dom.installBtn.disabled = true;
    const label = dom.installBtn.querySelector('.install-btn-text');
    if (label) label.textContent = 'INSTALLING...';
  }
  show(dom.installProgress);
  if (dom.progressText) dom.progressText.textContent = 'Starting download...';
  try {
    const res = await api('POST', '/install', { version_id: target });
    if (res.error) { showError(res.error); resetInstallUI(); return; }
    connectInstallSSE(res.install_id);
  } catch (err) {
    showError('Install failed: ' + err.message);
    resetInstallUI();
  }
}


function connectInstallSSE(installId) {
  if (state.installEventSource) state.installEventSource.close();
  const es = new EventSource(`${API}/install/${installId}/events`);
  state.installEventSource = es;
  const installTarget = dom.installBtn?.dataset.installTarget || state.selectedVersion?.id;
  const startTime = Date.now();

  // Show sidebar progress bar immediately
  updateSidebarProgress(installTarget, 0);

  es.addEventListener('progress', (e) => {
    const d = JSON.parse(e.data);
    let pct = 0;
    let label = '';

    // Phase weights: version_json=2%, client_jar=5%, libraries=13%, asset_index=1%, assets=72%, log_config=1%
    if (d.phase === 'version_json') {
      pct = 2; label = 'Fetching version info...';
    } else if (d.phase === 'client_jar') {
      pct = 7; label = 'Downloading game JAR...';
    } else if (d.phase === 'libraries') {
      const libPct = d.total > 0 ? d.current / d.total : 0;
      pct = 7 + Math.round(libPct * 13);
      label = `Libraries (${d.current}/${d.total})`;
    } else if (d.phase === 'asset_index') {
      pct = 21; label = 'Downloading asset index...';
    } else if (d.phase === 'assets') {
      const assetPct = d.total > 0 ? d.current / d.total : 0;
      pct = 21 + Math.round(assetPct * 72);
      label = `Assets (${d.current}/${d.total})`;
    } else if (d.phase === 'log_config') {
      pct = 94; label = 'Downloading log config...';
    } else if (d.phase === 'done') {
      pct = 100; label = 'Complete!';
    } else if (d.phase === 'error') {
      showError(d.error); updateSidebarProgress(installTarget, -1); onInstallDone(); return;
    }

    // ETA calculation
    if (pct > 5 && pct < 100) {
      const elapsed = (Date.now() - startTime) / 1000;
      const remaining = (elapsed / pct) * (100 - pct);
      if (remaining < 60) label += ` — ~${Math.ceil(remaining)}s left`;
      else label += ` — ~${Math.ceil(remaining / 60)}m left`;
    }

    if (dom.progressFill) dom.progressFill.style.width = pct + '%';
    if (dom.progressText) dom.progressText.textContent = label;
    updateSidebarProgress(installTarget, pct);

    if (d.done) onInstallDone();
  });
  es.onerror = () => { if (state.installing) { updateSidebarProgress(installTarget, -1); onInstallDone(); } };
}

function updateSidebarProgress(versionId, pct) {
  if (!versionId) return;
  const el = dom.versionList?.querySelector(`.version-item[data-id="${CSS.escape(versionId)}"]`);
  if (!el) return;
  let bar = el.querySelector('.version-install-bar');
  if (pct < 0) { if (bar) bar.remove(); return; }
  if (!bar) {
    bar = document.createElement('div');
    bar.className = 'version-install-bar';
    bar.innerHTML = '<div class="version-install-fill"></div>';
    el.appendChild(bar);
  }
  const fill = bar.querySelector('.version-install-fill');
  if (fill) fill.style.width = pct + '%';
  if (pct >= 100) setTimeout(() => bar.remove(), 1500);
}

async function onInstallDone() {
  state.installing = false;
  if (state.installEventSource) { state.installEventSource.close(); state.installEventSource = null; }
  if (dom.progressFill) dom.progressFill.style.width = '100%';
  if (dom.progressText) dom.progressText.textContent = 'Complete!';
  try {
    const res = await api('GET', '/versions');
    state.versions = res.versions || [];
    // Update catalog cache so newly installed version shows as installed
    if (state.catalog?.versions) {
      const installed = new Set(state.versions.filter(v => v.launchable).map(v => v.id));
      state.catalog.versions.forEach(v => { v.installed = installed.has(v.id); });
    }
    renderInstanceList();
    if (state.selectedInstance) renderSelectedInstance();
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

async function openNewInstanceFlow() {
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
        </div>
        <div>
          <label class="detail-prop-label" style="display:block;margin-bottom:6px;padding:0">Version</label>
          <input type="text" id="ni-version-search" class="search-input" placeholder="Search versions..." spellcheck="false" style="width:100%;box-sizing:border-box;margin-bottom:8px">
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
  const searchInput = document.getElementById('ni-version-search');
  const versionList = document.getElementById('ni-version-list');
  nameInput?.focus();

  function renderVersionPicker() {
    let list = allVersions.filter(v => v.type === filter);
    if (search) { const q = search.toLowerCase(); list = list.filter(v => v.id.toLowerCase().includes(q)); }
    const display = list.slice(0, 50);
    if (!display.length) {
      versionList.innerHTML = '<div style="padding:12px;text-align:center;color:var(--text-muted);font-size:12px">No versions found</div>';
      return;
    }
    versionList.innerHTML = display.map(v => {
      const selected = v.id === selectedVersionId;
      return `<div class="ni-version-item${selected ? ' selected' : ''}" data-vid="${esc(v.id)}"><span class="ni-version-id">${esc(v.id)}</span>${v.installed ? '<span class="ni-installed-badge">Installed</span>' : ''}</div>`;
    }).join('') + (list.length > 50 ? `<div style="padding:8px;text-align:center;font-size:10px;color:var(--text-muted)">Showing 50 of ${list.length}</div>` : '');
    versionList.querySelectorAll('.ni-version-item').forEach(el => {
      el.addEventListener('click', () => {
        selectedVersionId = el.dataset.vid;
        const cur = nameInput?.value.trim();
        if (!cur || allVersions.some(v => v.id === cur)) nameInput.value = selectedVersionId;
        renderVersionPicker();
        Sound.ui('click');
      });
    });
  }

  // Select first version by default
  const defaults = allVersions.filter(v => v.type === filter);
  if (defaults.length > 0) {
    selectedVersionId = defaults[0].id;
    if (!nameInput.value) nameInput.value = selectedVersionId;
  }
  renderVersionPicker();

  searchInput?.addEventListener('input', (e) => { search = e.target.value; renderVersionPicker(); });
  document.getElementById('ni-filters')?.querySelectorAll('.chip').forEach(chip => {
    chip.addEventListener('click', () => {
      document.getElementById('ni-filters').querySelectorAll('.chip').forEach(c => c.classList.remove('active'));
      chip.classList.add('active');
      filter = chip.dataset.nif;
      renderVersionPicker();
    });
  });

  const close = () => { modal.remove(); Sound.ui('soft'); };
  document.getElementById('new-instance-close')?.addEventListener('click', close);
  modal.addEventListener('click', (e) => { if (e.target === modal) close(); });

  document.getElementById('new-instance-create')?.addEventListener('click', async () => {
    const name = nameInput?.value.trim();
    if (!name) { nameInput?.focus(); return; }
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
      if (needsInstall) installVersion();
    } catch (err) {
      showError(err.message);
    }
  });

  nameInput?.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') document.getElementById('new-instance-create')?.click();
  });
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
  const inst = state.selectedInstance;
  if (!inst || state.gameRunning || state.launching) return;
  const version = state.versions.find(v => v.id === inst.version_id);
  if (!version?.launchable) return;
  Sound.init();

  const username = dom.usernameInput?.value.trim() || 'Player';
  const maxMemMB = Math.round(parseFloat(dom.memorySlider?.value || 4) * 1024);

  clearLaunchVisualState();
  state.launching = true;
  state.runningInstanceId = inst.id;
  state.runningVersionId = inst.version_id;
  state.activeSession = null;
  if (dom.launchSeqVersion) dom.launchSeqVersion.textContent = `${inst.name} (${inst.version_id})`;
  refreshSelectedInstanceActionState();
  startLaunchSequence();
  renderInstanceList();

  try {
    const res = await api('POST', '/launch', { instance_id: inst.id, username, max_memory_mb: maxMemMB });
    if (res.error) {
      showError(res.error);
      clearLaunchVisualState();
      state.launching = false;
      state.runningInstanceId = null;
      state.runningVersionId = null;
      refreshSelectedInstanceActionState();
      renderInstanceList();
      return;
    }

    state.activeSession = res.session_id;
    state.launching = false;
    state.gameRunning = true;

    endLaunchSequence();
    Sound.ui('launchSuccess');
    if (dom.runningVersion) dom.runningVersion.textContent = `${inst.name} (${inst.version_id})`;
    if (dom.runningPid) dom.runningPid.textContent = `PID ${res.pid}`;
    startRunningAnimation();
    startUptime();
    refreshSelectedInstanceActionState();
    renderInstanceList();
    dom.logPanel?.classList.add('expanded');
    connectLaunchSSE(res.session_id);

    // Update instance last-played in local state
    inst.last_played_at = res.launched_at || new Date().toISOString();
    if (state.config) {
      state.config.username = username;
      state.config.max_memory_mb = maxMemMB;
    }
  } catch (err) {
    showError(err.message);
    clearLaunchVisualState();
    state.launching = false;
    state.runningInstanceId = null;
    state.runningVersionId = null;
    refreshSelectedInstanceActionState();
    renderInstanceList();
  }
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
  state.runningInstanceId = null;
  state.runningVersionId = null;
  state.activeSession = null;
  if (state.eventSource) { state.eventSource.close(); state.eventSource = null; }
  clearLaunchVisualState();
  refreshSelectedInstanceActionState();
  appendLog('system', `Game exited with code ${exitCode}`);
  renderInstanceList();
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
  renderSelectedInstance();
  restoreFocusEl?.focus?.();
}

function syncSettingsForm() {
  if (state.config) {
    if (dom.settingJavaPath) dom.settingJavaPath.value = state.config.java_path_override || '';
    if (dom.settingWidth) dom.settingWidth.value = state.config.window_width || '';
    if (dom.settingHeight) dom.settingHeight.value = state.config.window_height || '';
    if (dom.jvmPresetGroup) {
      const preset = state.config.jvm_preset || '';
      const radio = dom.jvmPresetGroup.querySelector(`input[value="${preset}"]`);
      if (radio) radio.checked = true;
    }
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

  const presetRadio = dom.jvmPresetGroup?.querySelector('input[name="jvm-preset"]:checked');
  const preset = presetRadio?.value || '';
  if (preset !== (state.config?.jvm_preset || '')) updates.jvm_preset = preset;

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

// ── Setup (Minecraft not found) ──

function showSetup() {
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
  if (btn.id === 'settings-save' || btn.id === 'install-btn' || btn.id === 'onboarding-finish') return 'affirm';
  if (btn.id === 'settings-cancel' || btn.id === 'kill-btn' || btn.id === 'delete-cancel' || btn.id === 'delete-close') return 'soft';
  if (btn.id === 'delete-done-close') return 'affirm';
  if (btn.classList.contains('ctx-item')) return null; // handled by ctx menu
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
// CONTEXT MENU
// ══════════════════════════════════════════

let ctxMenuVersion = null;

function showInstanceContextMenu(e, inst) {
  e.preventDefault();
  ctxMenuVersion = { id: inst.id, _instance: inst }; // reuse ctxMenuVersion slot
  const menu = document.getElementById('ctx-menu');
  if (!menu) return;
  menu.classList.remove('hidden');
  const mw = menu.offsetWidth || 180;
  const mh = menu.offsetHeight || 120;
  let x = e.clientX;
  let y = e.clientY;
  if (x + mw > window.innerWidth - 8) x = window.innerWidth - mw - 8;
  if (y + mh > window.innerHeight - 8) y = window.innerHeight - mh - 8;
  if (x < 4) x = 4;
  if (y < 4) y = 4;
  menu.style.left = x + 'px';
  menu.style.top = y + 'px';
  Sound.ui('soft');
}

function showContextMenu(e, version) {
  e.preventDefault();
  ctxMenuVersion = version;
  const menu = document.getElementById('ctx-menu');
  if (!menu) return;
  menu.classList.remove('hidden');

  // Position: appear at cursor, but clamp to viewport
  const mw = menu.offsetWidth || 180;
  const mh = menu.offsetHeight || 120;
  let x = e.clientX;
  let y = e.clientY;
  if (x + mw > window.innerWidth - 8) x = window.innerWidth - mw - 8;
  if (y + mh > window.innerHeight - 8) y = window.innerHeight - mh - 8;
  if (x < 4) x = 4;
  if (y < 4) y = 4;
  menu.style.left = x + 'px';
  menu.style.top = y + 'px';

  Sound.ui('soft');
}

function hideContextMenu() {
  const menu = document.getElementById('ctx-menu');
  if (menu) menu.classList.add('hidden');
  ctxMenuVersion = null;
}

function bindContextMenu() {
  document.addEventListener('click', (e) => {
    const menu = document.getElementById('ctx-menu');
    if (menu && !menu.contains(e.target)) hideContextMenu();
  });
  document.addEventListener('contextmenu', (e) => {
    const menu = document.getElementById('ctx-menu');
    if (menu && !menu.classList.contains('hidden') && !menu.contains(e.target)) hideContextMenu();
  });

  document.getElementById('ctx-open-folder')?.addEventListener('click', () => {
    if (!ctxMenuVersion) return;
    const inst = ctxMenuVersion._instance;
    if (inst) {
      api('POST', `/instances/${encodeURIComponent(inst.id)}/open-folder`).catch(() => {});
    } else {
      api('POST', `/versions/${encodeURIComponent(ctxMenuVersion.id)}/open-folder`).catch(() => {});
    }
    hideContextMenu();
    Sound.ui('click');
  });

  document.getElementById('ctx-copy-id')?.addEventListener('click', () => {
    if (!ctxMenuVersion) return;
    const inst = ctxMenuVersion._instance;
    const text = inst ? inst.version_id : ctxMenuVersion.id;
    navigator.clipboard?.writeText(text).then(() => {
      Sound.ui('affirm');
    }).catch(() => {});
    hideContextMenu();
  });

  document.getElementById('ctx-rename')?.addEventListener('click', () => {
    if (!ctxMenuVersion?._instance) return;
    const inst = ctxMenuVersion._instance;
    hideContextMenu();
    const newName = prompt('Rename instance:', inst.name);
    if (newName && newName !== inst.name) {
      api('PUT', `/instances/${encodeURIComponent(inst.id)}`, { name: newName }).then(() => {
        inst.name = newName;
        renderInstanceList();
        if (state.selectedInstance?.id === inst.id) renderSelectedInstance();
      });
      Sound.ui('affirm');
    }
  });

  document.getElementById('ctx-delete')?.addEventListener('click', () => {
    if (!ctxMenuVersion) return;
    const inst = ctxMenuVersion._instance;
    if (inst) {
      hideContextMenu();
      if (!confirm(`Delete instance "${inst.name}"?\nThis will remove saves, mods, and all instance data.`)) return;
      api('DELETE', `/instances/${encodeURIComponent(inst.id)}`).then(res => {
        if (res.error) { showError(res.error); return; }
        state.instances = state.instances.filter(i => i.id !== inst.id);
        if (state.selectedInstance?.id === inst.id) {
          state.selectedInstance = null;
          dom.versionDetail?.classList.add('hidden');
          dom.emptyState?.classList.remove('hidden');
        }
        renderInstanceList();
        Sound.ui('affirm');
      });
    } else {
      const version = ctxMenuVersion;
      hideContextMenu();
      openDeleteWizard(version);
    }
  });
}

// ══════════════════════════════════════════
// DELETE VERSION WIZARD
// ══════════════════════════════════════════

let deleteTarget = null;
let deleteInfo = null;

function openDeleteWizard(version) {
  // Prevent deleting a running version
  if (state.gameRunning && state.runningVersionId === version.id) {
    showError(`Cannot delete ${version.id} while it's running. Stop the game first.`);
    return;
  }

  deleteTarget = version;
  deleteInfo = null;
  const modal = document.getElementById('delete-modal');
  if (!modal) return;

  // Reset all steps
  document.getElementById('delete-step-analyze')?.classList.remove('hidden');
  document.getElementById('delete-step-summary')?.classList.add('hidden');
  document.getElementById('delete-step-progress')?.classList.add('hidden');
  document.getElementById('delete-step-done')?.classList.add('hidden');

  const titleEl = document.getElementById('delete-modal-title');
  if (titleEl) titleEl.textContent = `Delete ${version.id}`;

  modal.classList.remove('hidden');
  Sound.ui('click');

  // Fetch version info
  fetchDeleteInfo(version.id);
}

function closeDeleteWizard() {
  const modal = document.getElementById('delete-modal');
  if (modal) modal.classList.add('hidden');
  deleteTarget = null;
  deleteInfo = null;
  const input = document.getElementById('delete-confirm-input');
  if (input) input.value = '';
}

async function fetchDeleteInfo(versionId) {
  try {
    const info = await api('GET', `/versions/${encodeURIComponent(versionId)}/info`);
    if (info.error) {
      closeDeleteWizard();
      showError(info.error);
      return;
    }
    deleteInfo = info;
    renderDeleteSummary();
  } catch (err) {
    closeDeleteWizard();
    showError('Failed to analyze version: ' + err.message);
  }
}

function formatBytes(bytes) {
  if (bytes < 1024) return bytes + ' B';
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
  if (bytes < 1024 * 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
  return (bytes / (1024 * 1024 * 1024)).toFixed(2) + ' GB';
}

function renderDeleteSummary() {
  if (!deleteInfo || !deleteTarget) return;

  document.getElementById('delete-step-analyze')?.classList.add('hidden');
  document.getElementById('delete-step-summary')?.classList.remove('hidden');

  const nameEl = document.getElementById('delete-version-name');
  if (nameEl) nameEl.textContent = deleteTarget.id;

  const sizeEl = document.getElementById('delete-version-size');
  if (sizeEl) sizeEl.textContent = formatBytes(deleteInfo.folder_size);

  // Dependents
  const deps = deleteInfo.dependents || [];
  const depCard = document.getElementById('delete-dependents-card');
  if (depCard) {
    depCard.classList.toggle('hidden', deps.length === 0);
    const depParent = document.getElementById('delete-dep-parent');
    if (depParent) depParent.textContent = deleteTarget.id;
    const depList = document.getElementById('delete-dep-list');
    if (depList) {
      depList.innerHTML = deps.map(d => `<span class="delete-dep-tag">${esc(d)}</span>`).join('');
    }
  }
  // Reset cascade checkbox
  const cascadeCheck = document.getElementById('delete-cascade-check');
  if (cascadeCheck) cascadeCheck.checked = false;

  // Worlds
  const worlds = deleteInfo.worlds || [];
  const worldCard = document.getElementById('delete-worlds-card');
  if (worldCard) {
    worldCard.classList.toggle('hidden', worlds.length === 0);
    const countEl = document.getElementById('delete-world-count');
    if (countEl) countEl.textContent = worlds.length;
    const worldList = document.getElementById('delete-world-list');
    if (worldList) {
      worldList.innerHTML = worlds.slice(0, 12).map(w =>
        `<span class="delete-world-tag">${esc(w.name)} <span class="delete-world-tag-size">${formatBytes(w.size)}</span></span>`
      ).join('') + (worlds.length > 12 ? `<span class="delete-world-tag">+${worlds.length - 12} more</span>` : '');
    }
  }

  // Shared data
  const shared = deleteInfo.shared_data || [];
  const sharedCard = document.getElementById('delete-shared-card');
  if (sharedCard) {
    sharedCard.classList.toggle('hidden', shared.length === 0);
    const sharedList = document.getElementById('delete-shared-list');
    if (sharedList) {
      sharedList.innerHTML = shared.map(s =>
        `<span class="delete-shared-tag">${esc(s.name)} <span class="delete-shared-tag-count">${s.count} items</span></span>`
      ).join('');
    }
  }

  // Folder path
  const folderEl = document.getElementById('delete-folder-path');
  if (folderEl) folderEl.textContent = `versions/${deleteTarget.id}/`;

  // Confirm target
  const confirmTarget = document.getElementById('delete-confirm-target');
  if (confirmTarget) confirmTarget.textContent = deleteTarget.id;

  // Reset confirm input and button
  const input = document.getElementById('delete-confirm-input');
  if (input) { input.value = ''; input.focus(); }
  const btn = document.getElementById('delete-confirm-btn');
  if (btn) btn.disabled = true;

  Sound.ui('bright');
}

function bindDeleteWizard() {
  // Confirm input validation
  document.getElementById('delete-confirm-input')?.addEventListener('input', (e) => {
    const btn = document.getElementById('delete-confirm-btn');
    if (!btn || !deleteTarget) return;
    btn.disabled = e.target.value !== deleteTarget.id;
  });

  // Enter key in confirm input
  document.getElementById('delete-confirm-input')?.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      const btn = document.getElementById('delete-confirm-btn');
      if (btn && !btn.disabled) executeDelete();
    }
  });

  // Delete button
  document.getElementById('delete-confirm-btn')?.addEventListener('click', executeDelete);

  // Cancel
  document.getElementById('delete-cancel')?.addEventListener('click', closeDeleteWizard);
  document.getElementById('delete-close')?.addEventListener('click', closeDeleteWizard);
  document.getElementById('delete-done-close')?.addEventListener('click', closeDeleteWizard);

  // Overlay click to close
  document.getElementById('delete-modal')?.addEventListener('click', (e) => {
    if (e.target.id === 'delete-modal') closeDeleteWizard();
  });
}

async function executeDelete() {
  if (!deleteTarget) return;

  const cascade = document.getElementById('delete-cascade-check')?.checked || false;
  const versionId = deleteTarget.id;

  // Show progress
  document.getElementById('delete-step-summary')?.classList.add('hidden');
  document.getElementById('delete-step-progress')?.classList.remove('hidden');

  const progressText = document.getElementById('delete-progress-text');
  if (progressText) progressText.textContent = cascade ? 'Deleting version and dependents...' : 'Deleting version...';

  Sound.ui('click');

  try {
    const res = await api('DELETE', `/versions/${encodeURIComponent(versionId)}`, { cascade_dependents: cascade });
    if (res.error) {
      closeDeleteWizard();
      showError(res.error);
      return;
    }

    // Show done
    document.getElementById('delete-step-progress')?.classList.add('hidden');
    document.getElementById('delete-step-done')?.classList.remove('hidden');

    const deleted = res.deleted || [versionId];
    const doneText = document.getElementById('delete-done-text');
    if (doneText) {
      if (deleted.length === 1) {
        doneText.textContent = `${deleted[0]} has been removed.`;
      } else {
        doneText.textContent = `Removed ${deleted.length} versions: ${deleted.join(', ')}`;
      }
    }

    Sound.ui('affirm');

    // Refresh version list
    try {
      const versionsRes = await api('GET', '/versions');
      state.versions = versionsRes.versions || [];
      // Refresh instance state after version deletion
      renderInstanceList();
      if (state.selectedInstance) renderSelectedInstance();
    } catch {}
  } catch (err) {
    closeDeleteWizard();
    showError('Delete failed: ' + err.message);
  }
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

  dom.addVersionBtn?.addEventListener('click', openNewInstanceFlow);
  dom.emptyAddBtn?.addEventListener('click', openNewInstanceFlow);

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
        state.selectedInstance = null;
        dom.versionDetail?.classList.add('hidden');
        dom.emptyState?.classList.remove('hidden');
        renderInstanceList();
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
      if (selVer?.launchable && !state.launching && !state.gameRunning) { Sound.ui('launchPress'); launchGame(); }
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
      // Close in priority order: context menu > delete wizard > new instance modal > settings
      const ctxMenu = document.getElementById('ctx-menu');
      const deleteModal = document.getElementById('delete-modal');
      const niModal = document.getElementById('new-instance-modal');
      if (ctxMenu && !ctxMenu.classList.contains('hidden')) hideContextMenu();
      else if (deleteModal && !deleteModal.classList.contains('hidden')) closeDeleteWizard();
      else if (niModal) { niModal.remove(); Sound.ui('soft'); }
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
