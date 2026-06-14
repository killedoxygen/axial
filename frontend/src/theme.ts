import { signal } from '@preact/signals';
import { defaults, local, saveLocalState, PRESET_HUES } from './state';
import { api } from './api';
import { config } from './store';
import { Sound } from './sound';
import { buildTheme, type Theme } from './tokens';
import { toast } from './toast';
import { windowSetResizeBackground } from './native';

const initialThemeHue = local.theme === 'custom' ? local.customHue : (PRESET_HUES[local.theme] ?? local.customHue);

export const themeSignal = signal<Theme>(
  buildTheme({
    dark: local.lightness < 50,
    hue: initialThemeHue,
    vibrancy: local.customVibrancy,
  }),
);

let lastNativeResizeBackgroundDark: boolean | null = null;

function syncNativeResizeBackground(dark: boolean): void {
  if (lastNativeResizeBackgroundDark === dark) return;
  lastNativeResizeBackgroundDark = dark;
  windowSetResizeBackground(dark).catch(() => {
    lastNativeResizeBackgroundDark = null;
  });
}

function chromaFor(vibrancy: number): number {
  return (0.14 * Math.max(0, Math.min(100, vibrancy))) / 100;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function wrapHue(hue: number): number {
  return ((hue % 360) + 360) % 360;
}

function signedHueDelta(from: number, to: number): number {
  return ((wrapHue(to) - wrapHue(from) + 540) % 360) - 180;
}

function applyLogoCssVars(set: (k: string, v: string) => void, hue: number, vibrancy: number): void {
  const hueDelta = signedHueDelta(140, hue);
  const saturation = clamp(vibrancy, 35, 100) / 100;
  const neutral = Math.abs(hueDelta) < 0.1 && saturation === 1;
  set('--logo-filter', neutral ? 'none' : `hue-rotate(${hueDelta}deg) saturate(${saturation})`);
}

function applyCssVars(hue: number, dark: boolean, vibrancy: number, deferLogo = false): void {
  const el = document.documentElement;
  const C = chromaFor(vibrancy);
  const Cf = 0.15 * Math.max(0.6, vibrancy / 100);
  const L = dark ? 0.78 : 0.62;
  const Lf = dark ? 0.58 : 0.52;

  const set = (k: string, v: string): void => el.style.setProperty(k, v);

  // Neutral chassis follows the accent hue at low chroma so the whole
  // surface stack harmonizes with the chosen accent.
  if (dark) {
    set('--bg-deep', `oklch(0.14 0.012 ${hue})`);
    set('--bg', `oklch(0.175 0.012 ${hue})`);
    set('--surface', `oklch(0.24 0.014 ${hue})`);
    set('--surface-2', `oklch(0.30 0.015 ${hue})`);
    set('--surface-3', `oklch(0.35 0.016 ${hue})`);
    set('--text', `oklch(0.96 0.005 ${hue})`);
    set('--text-dim', `oklch(0.74 0.010 ${hue})`);
    set('--text-mute', `oklch(0.58 0.012 ${hue})`);
  } else {
    set('--bg-deep', `oklch(0.92 0.008 ${hue})`);
    set('--bg', `oklch(0.95 0.006 ${hue})`);
    set('--surface', `oklch(0.995 0.003 ${hue})`);
    set('--surface-2', `oklch(0.945 0.006 ${hue})`);
    set('--surface-3', `oklch(0.905 0.008 ${hue})`);
    set('--text', `oklch(0.21 0.010 ${hue})`);
    set('--text-dim', `oklch(0.45 0.010 ${hue})`);
    set('--text-mute', `oklch(0.58 0.010 ${hue})`);
  }

  set('--accent', `oklch(${L} ${C} ${hue})`);
  set('--accent-strong', `oklch(${L - 0.08} ${C} ${hue})`);
  set('--accent-hover', `oklch(${Math.min(0.99, L + 0.04)} ${C} ${hue})`);
  set('--accent-fill', `oklch(${Lf} ${Cf} ${hue})`);
  set('--accent-fill-hover', `oklch(${Lf + 0.05} ${Cf} ${hue})`);
  set('--accent-on', `oklch(${dark ? 0.985 : 0.99} 0.015 ${hue})`);
  set('--accent-soft', `oklch(${L} ${C} ${hue} / 0.16)`);
  set('--accent-softer', `oklch(${L} ${C} ${hue} / 0.08)`);
  set('--accent-ring', `oklch(${L} ${C} ${hue} / 0.40)`);
  set('--accent-line', `oklch(${L} ${C} ${hue} / 0.28)`);
  if (!deferLogo) applyLogoCssVars(set, hue, vibrancy);

  el.setAttribute('data-color-mode', dark ? 'dark' : 'light');
}

interface ApplyOptions {
  silent?: boolean;
  vibrancy?: number;
  lightness?: number;
  deferLogo?: boolean;
  transient?: boolean;
}

export function applyTheme(theme: string, hue: number | null, options: ApplyOptions = {}): void {
  const { silent = false } = options;
  const transient = options.transient === true;

  const previousTheme = local.theme;
  const previousHue = local.customHue;
  const previousVibrancy = local.customVibrancy;
  const previousLightness = local.lightness;
  const previousResolvedHue = previousTheme === 'custom' ? previousHue : (PRESET_HUES[previousTheme] ?? previousHue);

  const lt = options.lightness ?? local.lightness;
  const vibrancy = options.vibrancy ?? local.customVibrancy;
  const dark = lt < 50;

  let resolvedHue: number;
  if (theme === 'custom') {
    resolvedHue = hue ?? local.customHue;
    if (!transient) {
      local.customHue = resolvedHue;
      local.customVibrancy = vibrancy;
    }
  } else {
    resolvedHue = PRESET_HUES[theme] ?? local.customHue;
  }

  applyCssVars(resolvedHue, dark, vibrancy, options.deferLogo === true);
  if (transient) return;

  themeSignal.value = buildTheme({ dark, hue: resolvedHue, vibrancy });
  syncNativeResizeBackground(dark);

  local.theme = theme;
  local.lightness = lt;

  if (!silent) {
    const payload: Record<string, unknown> = { theme, lightness: lt };
    if (theme === 'custom') {
      payload.custom_hue = resolvedHue;
      payload.custom_vibrancy = vibrancy;
    }
    api('PUT', '/config', payload)
      .then((r: any) => {
        if (r.error) throw new Error(r.error);
        config.value = r;
        saveLocalState();
        Sound.ui('theme');
      })
      .catch(() => {
        local.theme = previousTheme;
        local.customHue = previousHue;
        local.customVibrancy = previousVibrancy;
        local.lightness = previousLightness;
        const previousDark = previousLightness < 50;
        applyCssVars(previousResolvedHue, previousDark, previousVibrancy);
        themeSignal.value = buildTheme({
          dark: previousDark,
          hue: previousResolvedHue,
          vibrancy: previousVibrancy,
        });
        saveLocalState();
        toast('Failed to save theme', 'error');
      });
  }
}

export function resetThemeToDefault(): void {
  const previousTheme = local.theme;
  const previousHue = local.customHue;
  const previousVibrancy = local.customVibrancy;
  const previousLightness = local.lightness;
  const previousResolvedHue = previousTheme === 'custom' ? previousHue : (PRESET_HUES[previousTheme] ?? previousHue);

  const nextTheme = defaults.theme;
  const nextHue = defaults.customHue;
  const nextVibrancy = defaults.customVibrancy;
  const nextLightness = defaults.lightness;
  const nextDark = nextLightness < 50;

  applyCssVars(nextHue, nextDark, nextVibrancy);
  themeSignal.value = buildTheme({ dark: nextDark, hue: nextHue, vibrancy: nextVibrancy });
  syncNativeResizeBackground(nextDark);

  local.theme = nextTheme;
  local.customHue = nextHue;
  local.customVibrancy = nextVibrancy;
  local.lightness = nextLightness;

  api('PUT', '/config', {
    theme: nextTheme,
    lightness: nextLightness,
    custom_hue: nextHue,
    custom_vibrancy: nextVibrancy,
  })
    .then((r: any) => {
      if (r.error) throw new Error(r.error);
      config.value = r;
      saveLocalState();
      Sound.ui('theme');
    })
    .catch(() => {
      local.theme = previousTheme;
      local.customHue = previousHue;
      local.customVibrancy = previousVibrancy;
      local.lightness = previousLightness;
      const previousDark = previousLightness < 50;
      applyCssVars(previousResolvedHue, previousDark, previousVibrancy);
      themeSignal.value = buildTheme({
        dark: previousDark,
        hue: previousResolvedHue,
        vibrancy: previousVibrancy,
      });
      syncNativeResizeBackground(previousDark);
      saveLocalState();
      toast('Failed to reset theme', 'error');
    });
}

export function positionFieldMarker(
  field: HTMLElement | null,
  marker: HTMLElement | null,
  hue: number,
  vibrancy: number,
): void {
  if (!field || !marker) return;
  marker.style.left = `${(hue / 360) * 100}%`;
  marker.style.top = `${(1 - vibrancy / 100) * 100}%`;
  marker.style.background = `oklch(0.78 0.14 ${hue})`;
}

export function animateMarkerToHue(field: HTMLElement | null, marker: HTMLElement | null, hue: number): void {
  if (!field || !marker) return;
  marker.classList.add('animating');
  marker.style.left = `${(hue / 360) * 100}%`;
  marker.style.top = '0%';
  marker.style.background = `oklch(0.78 0.14 ${hue})`;
  setTimeout(() => marker.classList.remove('animating'), 380);
}

export function initColorField(
  field: HTMLElement | null,
  marker: HTMLElement | null,
  onDrag: (hue: number, vibrancy: number) => void,
  onEnd?: () => void,
): void {
  if (!field) return;
  let active = false;
  function calc(e: PointerEvent): { hue: number; vibrancy: number } {
    const r = field!.getBoundingClientRect();
    const x = Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
    const y = Math.max(0, Math.min(1, (e.clientY - r.top) / r.height));
    return { hue: Math.round(x * 360), vibrancy: Math.round((1 - y) * 100) };
  }
  field.addEventListener('pointerdown', (e: PointerEvent) => {
    active = true;
    field.setPointerCapture(e.pointerId);
    const c = calc(e);
    positionFieldMarker(field, marker, c.hue, c.vibrancy);
    onDrag(c.hue, c.vibrancy);
  });
  field.addEventListener('pointermove', (e: PointerEvent) => {
    if (!active) return;
    const c = calc(e);
    positionFieldMarker(field, marker, c.hue, c.vibrancy);
    onDrag(c.hue, c.vibrancy);
  });
  field.addEventListener('pointerup', () => {
    active = false;
    if (onEnd) onEnd();
  });
  field.addEventListener('lostpointercapture', () => {
    active = false;
  });
}
