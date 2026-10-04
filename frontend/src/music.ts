import { signal } from '@preact/signals';
import { apiResourceUrl } from './api';
import { canAutoSave, registerAutoSaveDraft, saveConfigPatch } from './hooks/use-autosave';
import { config } from './store';
import { toast } from './toast';

const DEFAULT_TRACK_COUNT = 2;
let trackCount = DEFAULT_TRACK_COUNT;

let audio: HTMLAudioElement | null = null;
let fadeRaf: number | null = null;
let fadeStart = 0;
let fadeFrom = 0;
let fadeTarget = 0;
let fadeCallback: (() => void) | null = null;
let persistTimer: ReturnType<typeof setTimeout> | null = null;
let persistPending = false;
let suppressed = false;
let pendingPlay: Promise<void> | null = null;
let pendingPlayVersion = 0;
let playbackVersion = 0;
let saveVersion = 0;
let acceptedMusic = { enabled: false, volume: 5, track: 0 };

const FADE_MS = 800;
export const musicStateVersion = signal(0);
export const musicError = signal<string | null>(null);

function playbackFailed(): void {
  if (!musicError.value) toast('Could not load background music. Try again.', 'error');
  musicError.value = 'Could not load background music. Try again.';
  Music.ready = false;
  notifyMusicState();
}

function notifyMusicState(): void {
  musicStateVersion.value += 1;
}

function clampTrack(track: number): number {
  if (trackCount <= 0) return 0;
  if (!Number.isFinite(track)) return 0;
  if (track < 0) return 0;
  if (track >= trackCount) return trackCount - 1;
  return Math.trunc(track);
}

function fadeStep(ts: number): void {
  if (!audio) {
    fadeRaf = null;
    if (fadeCallback) {
      const cb = fadeCallback;
      fadeCallback = null;
      cb();
    }
    return;
  }
  const t = Math.min(1, (ts - fadeStart) / FADE_MS);
  audio.volume = Math.max(0, Math.min(1, fadeFrom + (fadeTarget - fadeFrom) * t));
  if (t < 1) {
    fadeRaf = requestAnimationFrame(fadeStep);
  } else {
    fadeRaf = null;
    if (fadeCallback) {
      const cb = fadeCallback;
      fadeCallback = null;
      cb();
    }
  }
}

function cancelFade(): void {
  if (fadeRaf) {
    cancelAnimationFrame(fadeRaf);
    fadeRaf = null;
  }
  fadeCallback = null;
}

function startFade(target: number, cb?: () => void): void {
  cancelFade();
  if (!audio) {
    if (cb) cb();
    return;
  }
  fadeFrom = audio.volume;
  fadeTarget = target;
  fadeCallback = cb || null;
  fadeStart = performance.now();
  fadeRaf = requestAnimationFrame(fadeStep);
}

export const Music = {
  enabled: false,
  volume: 5,
  track: 0,
  ready: false,

  get targetVolume(): number {
    return this.volume / 100;
  },

  get playing(): boolean {
    return !!audio && !audio.paused;
  },

  applyConfig(cfg: { music_enabled?: boolean | null; music_volume?: number | null; music_track?: number }): void {
    if (cfg.music_enabled != null) this.enabled = cfg.music_enabled;
    if (cfg.music_volume != null && Number.isFinite(cfg.music_volume))
      this.volume = Math.max(0, Math.min(100, cfg.music_volume));
    if (cfg.music_track != null) this.track = clampTrack(cfg.music_track);
    acceptedMusic = { enabled: this.enabled, volume: this.volume, track: this.track };
    this.syncUI();
  },

  setTrackCount(count?: number): void {
    if (typeof count === 'number' && Number.isFinite(count) && count > 0) {
      trackCount = Math.max(1, Math.trunc(count));
    } else {
      trackCount = DEFAULT_TRACK_COUNT;
    }
    this.track = clampTrack(this.track);
    notifyMusicState();
  },

  persist(): void {
    if (persistTimer) clearTimeout(persistTimer);
    persistTimer = null;
    if (!canAutoSave()) {
      persistPending = true;
      return;
    }
    persistPending = false;
    const version = ++saveVersion;
    const preference = { enabled: this.enabled, volume: this.volume, track: this.track };
    void (async () => {
      try {
        await saveConfigPatch(
          {
            music_enabled: preference.enabled,
            music_volume: preference.volume,
            music_track: preference.track,
          },
          () => version === saveVersion,
        );
        if (version === saveVersion) acceptedMusic = preference;
      } catch {
        if (version !== saveVersion) return;
        const saved = config.value;
        if (saved)
          acceptedMusic = {
            enabled: saved.music_enabled ?? false,
            volume: saved.music_volume ?? 5,
            track: clampTrack(saved.music_track),
          };
        const trackChanged = this.track !== acceptedMusic.track;
        Object.assign(this, acceptedMusic);
        if (trackChanged && audio) {
          cancelFade();
          audio.pause();
          this.ready = false;
        }
        if (this.enabled && !suppressed) void this.play();
        else this.stop();
        this.syncUI();
        toast('Failed to save music preferences', 'error');
      }
    })();
  },

  debouncedPersist(): void {
    // Pending slider edits must not be rolled back by an older failed request.
    saveVersion += 1;
    persistPending = true;
    if (persistTimer) clearTimeout(persistTimer);
    persistTimer = setTimeout(() => {
      this.persist();
      persistTimer = null;
    }, 400);
  },

  toggle(): void {
    if (!canAutoSave()) return;
    this.enabled = !this.enabled;
    this.persist();
    if (this.enabled && !suppressed) void this.play();
    else if (!this.enabled) this.stop();
    this.syncUI();
  },

  setVolume(v: number): void {
    if (!canAutoSave()) {
      this.syncUI();
      return;
    }
    if (!Number.isFinite(v)) return;
    this.volume = Math.max(0, Math.min(100, v));
    if (audio && !suppressed) {
      if (fadeRaf) {
        fadeFrom = audio.volume;
        fadeTarget = this.targetVolume;
        fadeStart = performance.now();
      } else {
        audio.volume = this.targetVolume;
      }
    }
    this.debouncedPersist();
    this.syncUI();
  },

  async play(): Promise<void> {
    if (!this.enabled || suppressed) return;
    if (!audio) {
      audio = new Audio();
      audio.loop = true;
      audio.preload = 'none';
      audio.addEventListener('error', playbackFailed);
    }
    if (!this.ready) {
      audio.src = apiResourceUrl(`/music/track?t=${this.track}`);
      this.ready = true;
    }
    if (pendingPlay) {
      const superseded = pendingPlayVersion !== playbackVersion;
      await pendingPlay;
      if (superseded && this.enabled && !suppressed && audio.paused) return this.play();
      return;
    }
    if (!audio.paused) {
      startFade(this.targetVolume);
      return;
    }
    const version = playbackVersion;
    pendingPlayVersion = version;
    const player = audio;
    musicError.value = null;
    pendingPlay = (async () => {
      try {
        player.volume = 0;
        await player.play();
        if (version !== playbackVersion || !this.enabled || suppressed) {
          player.pause();
          return;
        }
        startFade(this.targetVolume);
        this.syncUI();
      } catch (error) {
        // Autoplay denial is expected until the first user interaction.
        if (version === playbackVersion && !(error instanceof DOMException && error.name === 'NotAllowedError'))
          playbackFailed();
      }
    })();
    try {
      await pendingPlay;
    } finally {
      pendingPlay = null;
    }
  },

  stop(): void {
    playbackVersion += 1;
    if (!audio || audio.paused) return;
    startFade(0, () => {
      audio!.pause();
      this.syncUI();
    });
  },

  nextTrack(): void {
    if (!canAutoSave()) return;
    playbackVersion += 1;
    this.track = (this.track + 1) % trackCount;
    this.ready = false;
    if (audio && !audio.paused) {
      startFade(0, () => {
        audio!.pause();
        audio!.src = apiResourceUrl(`/music/track?t=${this.track}`);
        this.ready = true;
        void this.play();
      });
    }
    this.persist();
    this.syncUI();
  },

  // Game-session suppression preserves the user's enabled preference.

  suppress(): void {
    if (suppressed) return;
    suppressed = true;
    this.stop();
    this.syncUI();
  },

  unsuppress(): void {
    if (!suppressed) return;
    suppressed = false;
    if (this.enabled && audio && !audio.paused) {
      startFade(this.targetVolume);
    } else if (this.enabled) {
      void this.play();
    }
    this.syncUI();
  },

  syncUI(): void {
    notifyMusicState();
  },
};

registerAutoSaveDraft(() => {
  if (persistPending) Music.persist();
});
