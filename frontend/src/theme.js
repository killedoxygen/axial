import { PRESET_HUES, LOGO_BASE_HUE, dom, local, state, saveLocalState } from './state.js';
import { api } from './api.js';
import { Sound } from './sound.js';

// generateThemeFromHue — exact copy from original lines 295-330
export function generateThemeFromHue(hue, vibrancy, lightness) {
  const v = (vibrancy != null ? vibrancy : 100) / 100;
  const l = (lightness != null ? lightness : 0) / 100;
  const baseSat = ((hue >= 0 && hue < 60) || hue >= 300 ? 18 : 15);
  const s = Math.round(baseSat * v);
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

// applyTheme — exact copy from original lines 332-371
export function applyTheme(theme, hue, options = {}) {
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

  // Persist to backend config (survives localStorage wipe)
  if (!silent) {
    const payload = { theme, lightness: lt };
    if (theme === 'custom') { payload.custom_hue = local.customHue; payload.custom_vibrancy = local.customVibrancy; }
    api('PUT', '/config', payload).then(r => { if (!r.error) state.config = r; }).catch(() => {});
  }

  el.style.setProperty('--logo-hue-shift', `${accentHue - LOGO_BASE_HUE}deg`);
  document.querySelectorAll('.lightness-slider').forEach(s => { s.value = lt; });
  dom.themePicker?.querySelectorAll('.theme-swatch').forEach(s => s.classList.toggle('active', s.dataset.theme === local.theme));

  if (theme !== 'custom' && PRESET_HUES[theme] != null) {
    animateMarkerToHue(dom.colorField, dom.colorFieldMarker, PRESET_HUES[theme]);
    animateMarkerToHue(dom.obColorField, dom.obColorFieldMarker, PRESET_HUES[theme]);
  }

  if (!silent) Sound.ui('theme');
}

// animateMarkerToHue — exact copy from lines 373-380
export function animateMarkerToHue(field, marker, hue) {
  if (!field || !marker) return;
  marker.classList.add('animating');
  marker.style.left = `${(hue / 360) * 100}%`;
  marker.style.top = '0%';
  marker.style.background = `hsl(${hue},65%,55%)`;
  setTimeout(() => marker.classList.remove('animating'), 380);
}

// positionFieldMarker — exact copy from lines 434-439
export function positionFieldMarker(field, marker, hue, vibrancy) {
  if (!field || !marker) return;
  marker.style.left = `${(hue / 360) * 100}%`;
  marker.style.top = `${(1 - vibrancy / 100) * 100}%`;
  marker.style.background = `hsl(${hue},65%,55%)`;
}

// initColorField — exact copy from lines 441-465
export function initColorField(field, marker, onDrag, onEnd) {
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
