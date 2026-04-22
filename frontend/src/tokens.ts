// Design tokens
// Accent engine derives a full scale from a single hue so user-chosen
// accents can't break contrast
// Mirrors the :root CSS variables in style.css, components can consume either

export interface AccentScale {
  base: string;
  strong: string;
  hover: string;
  fill: string;
  fillHover: string;
  soft: string;
  softer: string;
  ring: string;
  line: string;
  on: string;
}

export interface NeutralScale {
  bg: string;
  bgDeep: string;
  surface: string;
  surface2: string;
  surface3: string;
  line: string;
  lineStrong: string;
  text: string;
  textDim: string;
  textMute: string;
  shadow: string;
}

export interface Radii { xs: number; sm: number; md: number; lg: number; xl: number; }

export interface Theme {
  dark: boolean;
  hue: number;
  accent: AccentScale;
  n: NeutralScale;
  r: Radii;
  sp: (n: number) => number;
  ok: string;
  warn: string;
  err: string;
  info: string;
  font: { sans: string; mono: string };
}

export function buildAccent(hue: number, dark: boolean): AccentScale {
  const L = dark ? 0.78 : 0.62;
  const C = 0.14;
  const Lf = dark ? 0.58 : 0.52;
  const Cf = 0.15;
  return {
    base: `oklch(${L} ${C} ${hue})`,
    strong: `oklch(${L - 0.08} ${C} ${hue})`,
    hover: `oklch(${Math.min(0.99, L + 0.04)} ${C} ${hue})`,
    fill: `oklch(${Lf} ${Cf} ${hue})`,
    fillHover: `oklch(${Lf + 0.05} ${Cf} ${hue})`,
    soft: `oklch(${L} ${C} ${hue} / 0.16)`,
    softer: `oklch(${L} ${C} ${hue} / 0.08)`,
    ring: `oklch(${L} ${C} ${hue} / 0.40)`,
    line: `oklch(${L} ${C} ${hue} / 0.28)`,
    on: `oklch(${dark ? 0.985 : 0.99} 0.015 ${hue})`,
  };
}

export function buildNeutrals(dark: boolean): NeutralScale {
  if (dark) {
    return {
      bg: 'oklch(0.16 0.008 70)',
      bgDeep: 'oklch(0.12 0.008 70)',
      surface: 'oklch(0.20 0.008 70)',
      surface2: 'oklch(0.24 0.008 70)',
      surface3: 'oklch(0.28 0.008 70)',
      line: 'oklch(1 0 0 / 0.06)',
      lineStrong: 'oklch(1 0 0 / 0.12)',
      text: 'oklch(0.97 0.005 70)',
      textDim: 'oklch(0.72 0.008 70)',
      textMute: 'oklch(0.56 0.008 70)',
      shadow: '0 24px 60px -20px rgba(0,0,0,0.6), 0 2px 6px rgba(0,0,0,0.3)',
    };
  }
  return {
    bg: 'oklch(0.96 0.006 70)',
    bgDeep: 'oklch(0.94 0.006 70)',
    surface: 'oklch(0.99 0.004 70)',
    surface2: 'oklch(0.97 0.006 70)',
    surface3: 'oklch(0.93 0.008 70)',
    line: 'oklch(0 0 0 / 0.06)',
    lineStrong: 'oklch(0 0 0 / 0.12)',
    text: 'oklch(0.22 0.008 70)',
    textDim: 'oklch(0.45 0.008 70)',
    textMute: 'oklch(0.60 0.008 70)',
    shadow: '0 24px 60px -20px rgba(0,0,0,0.25), 0 2px 6px rgba(0,0,0,0.08)',
  };
}

export function buildTheme(opts: { dark?: boolean; hue?: number; radius?: number; density?: number } = {}): Theme {
  const dark = opts.dark ?? true;
  const hue = opts.hue ?? 70;
  const radius = opts.radius ?? 1;
  const density = opts.density ?? 1;
  return {
    dark,
    hue,
    accent: buildAccent(hue, dark),
    n: buildNeutrals(dark),
    r: {
      xs: 6 * radius,
      sm: 10 * radius,
      md: 14 * radius,
      lg: 20 * radius,
      xl: 28 * radius,
    },
    sp: (n: number) => n * 4 * density,
    ok: 'oklch(0.78 0.14 150)',
    warn: 'oklch(0.80 0.14 70)',
    err: 'oklch(0.70 0.18 25)',
    info: 'oklch(0.78 0.10 240)',
    font: {
      sans: '"Geist", ui-sans-serif, system-ui, -apple-system, sans-serif',
      mono: '"Geist", ui-sans-serif, system-ui, sans-serif',
    },
  };
}

// Deterministic hash used by Thumb and gradient helpers
export function hashStr(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

export function gradientFor(name: string, dark: boolean): { bg: string; hue1: number; hue2: number; angle: number; accent: string } {
  const h = hashStr(name || 'x');
  const hue1 = h % 360;
  const hue2 = (hue1 + 40 + ((h >> 8) % 80)) % 360;
  const angle = (h >> 4) % 360;
  const L1 = dark ? 0.32 : 0.72;
  const L2 = dark ? 0.22 : 0.86;
  const C = 0.12;
  return {
    hue1, hue2, angle,
    bg: `linear-gradient(${angle}deg, oklch(${L1} ${C} ${hue1}), oklch(${L2} ${C} ${hue2}))`,
    accent: `oklch(${L1} ${C} ${hue1})`,
  };
}
