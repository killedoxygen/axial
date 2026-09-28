import { signal } from '@preact/signals';
import { defaults, local, localStateVersion, saveLocalState, PRESET_HUES, canEditPreferences } from './state';
import { saveConfigPatch } from './hooks/use-autosave';
import { config } from './store';
import { Sound } from './sound';
import { buildTheme, type Theme } from './tokens';
import { toast } from './toast';
import { hasNativeDesktopRuntime, windowSetResizeBackground } from './native';
import { flushNativePreferences, nativePreferencesHydrated } from './preferences/persistence';
import type { Config } from './types-settings';

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

type ThemePreference = Pick<typeof local, 'theme' | 'customHue' | 'customVibrancy' | 'lightness'>;

function currentPreference(): ThemePreference {
  return { theme: local.theme, customHue: local.customHue, customVibrancy: local.customVibrancy, lightness: local.lightness };
}

let acceptedPreference = currentPreference();
let themeSaveVersion = 0;

function restorePreference(preference: ThemePreference): void {
  Object.assign(local, preference);
  const hue = preference.theme === 'custom' ? preference.customHue : (PRESET_HUES[preference.theme] ?? preference.customHue);
  const dark = preference.lightness < 50;
  applyCssVars(hue, dark, preference.customVibrancy);
  themeSignal.value = buildTheme({ dark, hue, vibrancy: preference.customVibrancy });
  syncNativeResizeBackground(dark);
}

function persistTheme(payload: Record<string, unknown>, failureMessage: string): void {
  if (hasNativeDesktopRuntime()) {
    saveLocalState();
    Sound.ui('theme');
    return;
  }
  const version = ++themeSaveVersion;
  const preference = currentPreference();
  void (async () => {
    try {
      await saveConfigPatch(payload);
      acceptedPreference = preference;
      if (version === themeSaveVersion) {
        saveLocalState();
        Sound.ui('theme');
      }
    } catch {
      if (version !== themeSaveVersion) return;
      const saved = config.value;
      if (saved?.theme) acceptedPreference = {
        theme: saved.theme, customHue: saved.custom_hue ?? defaults.customHue,
        customVibrancy: saved.custom_vibrancy ?? defaults.customVibrancy, lightness: saved.lightness ?? defaults.lightness,
      };
      restorePreference(acceptedPreference);
      saveLocalState();
      toast(failureMessage, 'error');
    }
  })();
}

export function applyTheme(theme: string, hue: number | null, options: ApplyOptions = {}): void {
  if ((!options.silent || options.transient) && !canEditPreferences()) return;
  if (hasNativeDesktopRuntime() && !nativePreferencesHydrated()) return;
  const { silent = false } = options;
  const transient = options.transient === true;

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
    persistTheme(payload, 'Failed to save theme');
  } else {
    acceptedPreference = currentPreference();
  }
}

export function applyConfigTheme(cfg: Config): void {
  if (hasNativeDesktopRuntime()) return;
  if (local.theme !== 'obsidian' || !cfg.theme || cfg.theme === 'obsidian') return;
  applyTheme(cfg.theme, cfg.custom_hue ?? local.customHue, {
    silent: true,
    vibrancy: cfg.custom_vibrancy ?? local.customVibrancy,
    lightness: cfg.lightness ?? local.lightness,
  });
}

export async function applyImportedConfigTheme(cfg: Config, preferenceVersion: number): Promise<void> {
  if (!hasNativeDesktopRuntime()) { applyConfigTheme(cfg); return; }
  if (!canEditPreferences()) throw new Error('Interface preferences are paused. Refresh the imported settings again.');
  if (localStateVersion.value !== preferenceVersion) return;
  if (local.theme === 'obsidian' && cfg.theme && cfg.theme !== 'obsidian') {
    applyTheme(cfg.theme, cfg.custom_hue ?? local.customHue, {
      silent: true, vibrancy: cfg.custom_vibrancy ?? local.customVibrancy,
      lightness: cfg.lightness ?? local.lightness,
    });
    saveLocalState();
  }
  await flushNativePreferences();
}

export function resetThemeToDefault(): void {
  if (!canEditPreferences()) return;
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

  persistTheme({
    theme: nextTheme,
    lightness: nextLightness,
    custom_hue: nextHue,
    custom_vibrancy: nextVibrancy,
  }, 'Failed to reset theme');
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
