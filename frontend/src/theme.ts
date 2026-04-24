// Theme engine
// User picks a hue (0..359), vibrancy (0..100), and a light/dark mode
// Vars mirror the :root block in style.css, components consume either
// those CSS vars or the Theme object from tokens.ts (rebuilt on every change)
import { signal } from '@preact/signals';
import { local, saveLocalState, PRESET_HUES } from './state';
import { api } from './api';
import { config } from './store';
import { Sound } from './sound';
import { buildTheme, type Theme } from './tokens';

// ── Reactive theme snapshot ──────────────────────────────────────────────────

export const themeSignal = signal<Theme>(buildTheme({ dark: true, hue: 70 }));

// Vibrancy is a chroma multiplier, 0..100, 100 gives full chroma of 0.14
// Lightness is 0..100 where 0 is dark and 100 is light, we snap at 50

function chromaFor(vibrancy: number): number {
  return 0.14 * Math.max(0, Math.min(100, vibrancy)) / 100;
}

function applyCssVars(hue: number, dark: boolean, vibrancy: number): void {
  const el = document.documentElement;
  const C = chromaFor(vibrancy);
  const Cf = 0.15 * Math.max(0.6, vibrancy / 100);
  const L = dark ? 0.78 : 0.62;
  const Lf = dark ? 0.58 : 0.52;

  const set = (k: string, v: string): void => el.style.setProperty(k, v);

  // Accent scale (hue-driven).
  set('--accent',           `oklch(${L} ${C} ${hue})`);
  set('--accent-strong',    `oklch(${L - 0.08} ${C} ${hue})`);
  set('--accent-hover',     `oklch(${Math.min(0.99, L + 0.04)} ${C} ${hue})`);
  set('--accent-fill',      `oklch(${Lf} ${Cf} ${hue})`);
  set('--accent-fill-hover',`oklch(${Lf + 0.05} ${Cf} ${hue})`);
  set('--accent-on',        `oklch(${dark ? 0.985 : 0.99} 0.015 ${hue})`);
  set('--accent-soft',      `oklch(${L} ${C} ${hue} / 0.16)`);
  set('--accent-softer',    `oklch(${L} ${C} ${hue} / 0.08)`);
  set('--accent-ring',      `oklch(${L} ${C} ${hue} / 0.40)`);
  set('--accent-line',      `oklch(${L} ${C} ${hue} / 0.28)`);

  el.setAttribute('data-color-mode', dark ? 'dark' : 'light');
  el.setAttribute('data-theme', 'custom');
}

interface ApplyOptions {
  silent?: boolean;
  vibrancy?: number;
  lightness?: number;
}

export function applyTheme(theme: string, hue: number | null, options: ApplyOptions = {}): void {
  const { silent = false } = options;

  const lt = options.lightness ?? local.lightness;
  const vibrancy = options.vibrancy ?? local.customVibrancy;
  const dark = lt < 50;

  let resolvedHue: number;
  if (theme === 'custom') {
    resolvedHue = hue ?? local.customHue;
    local.customHue = resolvedHue;
    local.customVibrancy = vibrancy;
  } else {
    resolvedHue = PRESET_HUES[theme] ?? local.customHue;
  }

  applyCssVars(resolvedHue, dark, vibrancy);
  themeSignal.value = buildTheme({ dark, hue: resolvedHue });

  local.theme = theme;
  local.lightness = lt;
  saveLocalState();

  if (!silent) {
    const payload: Record<string, unknown> = { theme, lightness: lt };
    if (theme === 'custom') {
      payload.custom_hue = resolvedHue;
      payload.custom_vibrancy = vibrancy;
    }
    api('PUT', '/config', payload).then((r: any) => { if (!r.error) config.value = r; }).catch(() => {});
    Sound.ui('theme');
  }
}

// ── Color field (hue × vibrancy picker). Pointer-driven square surface. ─────

export function positionFieldMarker(field: HTMLElement | null, marker: HTMLElement | null, hue: number, vibrancy: number): void {
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
  field.addEventListener('pointerup', () => { active = false; if (onEnd) onEnd(); });
  field.addEventListener('lostpointercapture', () => { active = false; });
}
