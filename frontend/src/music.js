import { dom } from './state.js';
import { api, API } from './api.js';

let audio = null;
let fadeRaf = null;
let fadeStart = 0;
let fadeFrom = 0;
let fadeTarget = 0;
let fadeCallback = null;
let persistTimer = null;
let iconOn = null;
let iconOff = null;
let suppressed = false;

const FADE_MS = 800;

function fadeStep(ts) {
  if (!audio) {
    fadeRaf = null;
    if (fadeCallback) { const cb = fadeCallback; fadeCallback = null; cb(); }
    return;
  }
  const t = Math.min(1, (ts - fadeStart) / FADE_MS);
  audio.volume = Math.max(0, Math.min(1, fadeFrom + (fadeTarget - fadeFrom) * t));
  if (t < 1) {
    fadeRaf = requestAnimationFrame(fadeStep);
  } else {
    fadeRaf = null;
    if (fadeCallback) { const cb = fadeCallback; fadeCallback = null; cb(); }
  }
}

function cancelFade() {
  if (fadeRaf) { cancelAnimationFrame(fadeRaf); fadeRaf = null; }
  fadeCallback = null;
}

function startFade(target, cb) {
  cancelFade();
  if (!audio) { if (cb) cb(); return; }
  fadeFrom = audio.volume;
  fadeTarget = target;
  fadeCallback = cb || null;
  fadeStart = performance.now();
  fadeRaf = requestAnimationFrame(fadeStep);
}

export const Music = {
  enabled: false,
  volume: 5,
  ready: false,

  /** Resolved target volume (0–1). */
  get targetVolume() { return this.volume / 100; },

  /** Whether audio is actively producing sound (not paused, not suppressed at 0). */
  get playing() { return !!audio && !audio.paused; },

  // ── Configuration ──

  applyConfig(cfg) {
    if (cfg.music_enabled != null) this.enabled = cfg.music_enabled;
    if (cfg.music_volume != null) this.volume = cfg.music_volume;
    this.syncUI();
  },

  persist() {
    api('PUT', '/config', { music_enabled: this.enabled, music_volume: this.volume }).catch(() => {});
  },

  debouncedPersist() {
    clearTimeout(persistTimer);
    persistTimer = setTimeout(() => { this.persist(); persistTimer = null; }, 400);
  },

  // ── Playback ──

  toggle() {
    this.enabled = !this.enabled;
    this.persist();
    if (this.enabled && !suppressed) this.play();
    else if (!this.enabled) this.stop();
    this.syncUI();
  },

  setVolume(v) {
    this.volume = Math.max(0, Math.min(100, v));
    if (audio && !fadeRaf && !suppressed) audio.volume = this.targetVolume;
    this.debouncedPersist();
  },

  async play() {
    if (!this.enabled || suppressed) return;
    if (!audio) {
      audio = new Audio();
      audio.loop = true;
      audio.preload = 'none';
    }
    if (!this.ready) {
      audio.src = `${API}/music/track`;
      this.ready = true;
    }
    if (!audio.paused) return;
    try {
      audio.volume = 0;
      await audio.play();
      startFade(this.targetVolume);
      this.syncUI();
    } catch { /* autoplay blocked — expected before interaction */ }
  },

  stop() {
    if (!audio || audio.paused) return;
    startFade(0, () => { audio.pause(); this.syncUI(); });
  },

  // ── Game session suppression ──
  // Fades music out while any game instance is running.
  // Does NOT touch `enabled` — the user's preference is preserved.

  suppress() {
    if (suppressed || !this.enabled) return;
    suppressed = true;
    if (audio && !audio.paused) startFade(0);
    this.syncUI();
  },

  unsuppress() {
    if (!suppressed) return;
    suppressed = false;
    if (this.enabled && audio && !audio.paused) {
      startFade(this.targetVolume);
    } else if (this.enabled) {
      this.play();
    }
    this.syncUI();
  },

  // ── UI sync ──

  /** Update header icon, equalizer, and settings form to reflect current state. */
  syncUI() {
    const btn = dom.musicBtn;
    if (btn) {
      btn.classList.toggle('active', this.enabled);
      btn.title = this.enabled ? (suppressed ? 'Music (paused for game)' : 'Music on') : 'Music off';
      if (!iconOn) { iconOn = btn.querySelector('.music-icon-on'); iconOff = btn.querySelector('.music-icon-off'); }
      if (iconOn) iconOn.style.display = this.enabled ? '' : 'none';
      if (iconOff) iconOff.style.display = this.enabled ? 'none' : '';
    }
    // Equalizer: visible only when actually producing audible output
    const audible = this.enabled && this.playing && !suppressed;
    if (dom.musicEq) dom.musicEq.classList.toggle('hidden', !audible);

    // Settings form (if open)
    if (dom.musicToggle) dom.musicToggle.checked = this.enabled;
    if (dom.musicVolumeSlider) dom.musicVolumeSlider.value = this.volume;
    if (dom.musicVolumeValue) dom.musicVolumeValue.textContent = `${this.volume}%`;
    if (dom.musicVolumeRow) dom.musicVolumeRow.style.display = this.enabled ? '' : 'none';
  },
};
