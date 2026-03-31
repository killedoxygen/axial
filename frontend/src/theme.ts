import { PRESET_HUES, LOGO_BASE_HUE, local, saveLocalState } from './state';
import { api } from './api';
import { byId } from './dom';
import { Sound } from './sound';
import { config } from './store';

// WCAG 2.1 contrast helpers
function srgbToLinear(c: number): number {
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
}

function hslToLuminance(h: number, s: number, l: number): number {
  s /= 100; l /= 100;
  const a = s * Math.min(l, 1 - l);
  const f = (n: number): number => { const k = (n + h / 30) % 12; return l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1)); };
  return 0.2126 * srgbToLinear(f(0)) + 0.7152 * srgbToLinear(f(8)) + 0.0722 * srgbToLinear(f(4));
}

function contrastRatio(l1: number, l2: number): number {
  const [a, b] = l1 > l2 ? [l1, l2] : [l2, l1];
  return (a + 0.05) / (b + 0.05);
}

function parseHSL(str: string): [number, number, number] | null {
  const m = str.match(/hsl\((\d+),(\d+)%,([.\d]+)%\)/);
  return m ? [+m[1], +m[2], +m[3]] : null;
}

function checkContrast(vars: Record<string, string>): number {
  const text = parseHSL(vars['--text']);
  const bg = parseHSL(vars['--bg']);
  if (!text || !bg) return Infinity;
  return contrastRatio(hslToLuminance(...text), hslToLuminance(...bg));
}

export function isLowContrastTheme(hue: number, vibrancy: number, lightness: number): boolean {
  return checkContrast(generateThemeFromHue(hue, vibrancy, lightness)) < 4.5;
}

export function findFixedLightness(hue: number, vibrancy: number, currentLt: number): number {
  const dir = currentLt < 50 ? -1 : 1;
  for (let lt = currentLt; lt >= 0 && lt <= 100; lt += dir) {
    const vars = generateThemeFromHue(hue, vibrancy, lt);
    if (checkContrast(vars) >= 4.5) return lt;
  }
  return dir > 0 ? 100 : 0;
}

// generateThemeFromHue. Exact copy from original lines 295 to 330.
export function generateThemeFromHue(hue: number, vibrancy: number, lightness: number): Record<string, string> {
  const v = (vibrancy != null ? vibrancy : 100) / 100;
  const l = (lightness != null ? lightness : 0) / 100;
  const baseSat = ((hue >= 0 && hue < 60) || hue >= 300 ? 18 : 15);
  const s = Math.round(baseSat * v);
  const mix = (dark: number, light: number): number => Math.round(dark + (light - dark) * l);
  const mixF = (dark: number, light: number): number => +(dark + (light - dark) * l).toFixed(2);

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

interface ApplyThemeOptions {
  silent?: boolean;
  vibrancy?: number;
  lightness?: number;
}

// applyTheme. Exact copy from original lines 332 to 371.
export function applyTheme(theme: string, hue: number | null, options: ApplyThemeOptions = {}): void {
  const { silent = false, vibrancy, lightness } = options;
  const el = document.documentElement;
  const clearVars = (): void => { Object.keys(generateThemeFromHue(0, 100, 0)).forEach(k => el.style.removeProperty(k)); };
  const lt: number = lightness ?? local.lightness;
  clearVars();

  let accentHue: number = PRESET_HUES[theme] ?? local.customHue;
  let generatedVars: Record<string, string> | null = null;
  if (theme === 'custom' || lt > 0) {
    const h: number = theme === 'custom' ? (hue ?? local.customHue) : (PRESET_HUES[theme] || 140);
    const v: number = vibrancy ?? local.customVibrancy;
    accentHue = h;
    el.setAttribute('data-theme', 'custom');
    generatedVars = generateThemeFromHue(h, v, lt);
    Object.entries(generatedVars).forEach(([k, val]) => el.style.setProperty(k, val));
    if (theme === 'custom') { local.customHue = h; local.customVibrancy = v; }
  } else {
    el.setAttribute('data-theme', theme);
  }

  // WCAG contrast check for custom/adjusted themes
  const warn = document.getElementById('wcag-warning');
  if (warn) {
    if (generatedVars && checkContrast(generatedVars) < 4.5) {
      warn.classList.remove('hidden');
    } else {
      warn.classList.add('hidden');
    }
  }

  el.setAttribute('data-color-mode', lt >= 50 ? 'light' : 'dark');
  local.lightness = lt;
  local.theme = theme;
  saveLocalState();

  // Persist to backend config (survives localStorage wipe)
  if (!silent) {
    const payload: Record<string, any> = { theme, lightness: lt };
    if (theme === 'custom') { payload.custom_hue = local.customHue; payload.custom_vibrancy = local.customVibrancy; }
    api('PUT', '/config', payload).then((r: any) => { if (!r.error) config.value = r; }).catch(() => {});
  }

  el.style.setProperty('--logo-hue-shift', `${accentHue - LOGO_BASE_HUE}deg`);
  document.querySelectorAll('.lightness-slider').forEach(s => { (s as HTMLInputElement).value = String(lt); });
  byId<HTMLElement>('theme-picker')?.querySelectorAll('.theme-swatch').forEach(s => (s as HTMLElement).classList.toggle('active', (s as HTMLElement).dataset.theme === local.theme));

  if (theme !== 'custom' && PRESET_HUES[theme] != null) {
    animateMarkerToHue(byId('color-field'), byId('color-field-marker'), PRESET_HUES[theme]);
    animateMarkerToHue(byId('ob-color-field'), byId('ob-color-field-marker'), PRESET_HUES[theme]);
  }

  if (!silent) Sound.ui('theme');
}

// animateMarkerToHue. Exact copy from lines 373 to 380.
export function animateMarkerToHue(field: HTMLElement | null, marker: HTMLElement | null, hue: number): void {
  if (!field || !marker) return;
  marker.classList.add('animating');
  marker.style.left = `${(hue / 360) * 100}%`;
  marker.style.top = '0%';
  marker.style.background = `hsl(${hue},65%,55%)`;
  setTimeout(() => marker.classList.remove('animating'), 380);
}

// positionFieldMarker. Exact copy from lines 434 to 439.
export function positionFieldMarker(field: HTMLElement | null, marker: HTMLElement | null, hue: number, vibrancy: number): void {
  if (!field || !marker) return;
  marker.style.left = `${(hue / 360) * 100}%`;
  marker.style.top = `${(1 - vibrancy / 100) * 100}%`;
  marker.style.background = `hsl(${hue},65%,55%)`;
}

// initColorField. Exact copy from lines 441 to 465.
export function initColorField(field: HTMLElement | null, marker: HTMLElement | null, onDrag: (hue: number, vibrancy: number) => void, onEnd?: () => void): void {
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
