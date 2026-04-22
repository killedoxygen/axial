let lastMemorySoundAt = 0;
let lastHueSoundAt = 0;

interface SpriteEntry {
  start: number;
  end: number;
}

interface SpriteMap {
  [name: string]: SpriteEntry;
}

interface PlayBufferOptions {
  when?: number;
  volume?: number;
  playbackRate?: number;
  offset?: number;
  duration?: number | null;
}

interface ToneOptions {
  type?: OscillatorType;
  volume?: number;
  when?: number;
  attack?: number;
  release?: number;
  detune?: number;
  endFreq?: number | null;
}

interface NoteDescriptor extends ToneOptions {
  freq: number;
  duration: number;
}

type SoundKind = 'soft' | 'bright' | 'affirm' | 'theme' | 'slider' | 'memory' | 'launchPress' | 'launchSuccess' | 'click';

export const Sound = {
  ctx: null as AudioContext | null,
  enabled: true,
  preloadPromise: null as Promise<void> | null,
  spriteBuffer: null as AudioBuffer | null,
  spriteMap: null as SpriteMap | null,
  customBuffers: new Map<string, AudioBuffer>(),
  init(): AudioContext | null {
    if (this.ctx) return this.ctx;
    try {
      this.ctx = new (window.AudioContext || (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext)();
    } catch {}
    return this.ctx;
  },
  activate(): void {
    this.init();
    this.preload();
    if (this.ctx?.state === 'suspended') this.ctx.resume().catch(() => {});
  },
  preload(): Promise<void> {
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
        this.spriteMap = (manifest.spritemap || {}) as SpriteMap;
        this.spriteBuffer = await this.ctx!.decodeAudioData(spriteArray.slice(0));
        this.customBuffers.set('launchSuccess', await this.ctx!.decodeAudioData(launchArray.slice(0)));
      } catch {}
    })();
    return this.preloadPromise;
  },
  async warmup(): Promise<void> {
    this.activate();
    try { await this.preload(); } catch {}
  },
  playBuffer(buffer: AudioBuffer | undefined, options: PlayBufferOptions = {}): boolean {
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
  playSprite(name: string, options: PlayBufferOptions = {}): boolean {
    const entry = this.spriteMap?.[name];
    if (!entry || !this.spriteBuffer) return false;
    return this.playBuffer(this.spriteBuffer, {
      offset: entry.start,
      duration: Math.max(0.01, entry.end - entry.start),
      ...options,
    });
  },
  randomFrom(keys: string[]): string {
    return keys[Math.floor(Math.random() * keys.length)];
  },
  playKind(kind: SoundKind, value: number = 0.5): boolean {
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
  tone(freq: number, duration: number, options: ToneOptions = {}): void {
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
  sequence(notes: NoteDescriptor[]): void { notes.forEach((note: NoteDescriptor) => this.tone(note.freq, note.duration, note)); },
  ui(kind: SoundKind, value: number = 0.5): void {
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
  tick(): void { this.ui('click'); },
};

export function inferButtonSound(btn: HTMLElement): SoundKind | null {
  // Skip buttons that play their own tailored sound
  if (btn.dataset.soundSilent === 'true') return null;
  if (btn.classList.contains('cp-winctrl')) return 'soft';
  if (btn.classList.contains('cp-sidebar-item')) return 'soft';
  if (btn.classList.contains('cp-seg') || btn.closest('.cp-seg')) return 'soft';
  if (btn.classList.contains('cp-ob-choice')) return 'soft';

  // Button variants drive the sound directly.
  if (btn.classList.contains('cp-btn--primary')) {
    const label = btn.textContent?.toLowerCase() || '';
    if (label.includes('play') || label.includes('launch') || label.includes('resume')) return 'launchPress';
    if (label.includes('create') || label.includes('finish') || label.includes('save') || label.includes('continue') || label.includes('confirm')) return 'affirm';
    return 'bright';
  }
  if (btn.classList.contains('cp-btn--danger')) return 'soft';
  if (btn.classList.contains('cp-btn--soft')) return 'soft';
  if (btn.classList.contains('cp-btn--ghost')) return 'soft';
  if (btn.classList.contains('cp-btn--secondary')) return 'click';

  if (btn.classList.contains('cp-ibtn')) return 'click';
  if (btn.classList.contains('cp-quick-action')) return 'click';
  if (btn.classList.contains('cp-status-pill--running')) return 'soft';
  if (btn.classList.contains('cp-userm-row')) return 'soft';
  if (btn.classList.contains('cp-music-btn')) return 'soft';
  if (btn.classList.contains('cp-swatch')) return 'soft';

  return 'click';
}

export function bindButtonSounds(): void {
  document.addEventListener('click', (e: MouseEvent) => {
    const btn = (e.target as Element | null)?.closest('button') as HTMLButtonElement | null;
    if (!btn || btn.disabled) return;
    const kind = inferButtonSound(btn);
    if (kind) Sound.ui(kind);
  });
}

export function playSliderSound(value: number, family: string): void {
  const now = performance.now();
  const limit = family === 'memory' ? 55 : 45;
  const ref = family === 'memory' ? lastMemorySoundAt : lastHueSoundAt;
  if (now - ref < limit) return;
  if (family === 'memory') lastMemorySoundAt = now;
  else lastHueSoundAt = now;
  Sound.ui(family === 'memory' ? 'memory' : 'slider', Math.max(0, Math.min(1, value)));
}
