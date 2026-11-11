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

export interface Radii {
  xs: number;
  sm: number;
  md: number;
  lg: number;
  xl: number;
}

export interface Theme {
  dark: boolean;
  hue: number;
  accent: AccentScale;
  n: NeutralScale;
  r: Radii;
  ok: string;
  warn: string;
  err: string;
  info: string;
  font: { sans: string; mono: string };
}

export function buildAccent(hue: number, dark: boolean, vibrancy = 100): AccentScale {
  const L = dark ? 0.78 : 0.62;
  const clampedVibrancy = Math.max(0, Math.min(100, vibrancy));
  const C = (0.14 * clampedVibrancy) / 100;
  const Lf = dark ? 0.58 : 0.52;
  const Cf = 0.15 * Math.max(0.6, clampedVibrancy / 100);
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

/** The neutral ramp as CSS custom properties, tinted by the accent hue. `base.css` holds the same values for hue 140. */
export function neutralVars(dark: boolean, hue: number): Record<string, string> {
  const ramp = dark
    ? {
        '--bg-deep': '0.155 0.012',
        '--bg': '0.185 0.012',
        '--tile': '0.218 0.013',
        '--fill': '0.232 0.013',
        '--fill-2': '0.272 0.014',
        '--fill-3': '0.33 0.015',
        '--pop': '0.248 0.014',
        '--text': '0.96 0.005',
        '--text-dim': '0.74 0.01',
        '--text-mute': '0.58 0.012',
      }
    : {
        '--bg-deep': '0.925 0.008',
        '--bg': '0.975 0.004',
        '--tile': '0.948 0.006',
        '--fill': '0.935 0.007',
        '--fill-2': '0.905 0.008',
        '--fill-3': '0.87 0.009',
        '--pop': '0.995 0.003',
        '--text': '0.21 0.01',
        '--text-dim': '0.45 0.01',
        '--text-mute': '0.58 0.01',
      };
  return Object.fromEntries(Object.entries(ramp).map(([name, lc]) => [name, `oklch(${lc} ${hue})`]));
}

export function buildNeutrals(dark: boolean, hue = 140): NeutralScale {
  const vars = neutralVars(dark, hue);
  const ink = dark ? '1 0 0' : '0 0 0';
  return {
    bg: vars['--bg'],
    bgDeep: vars['--bg-deep'],
    surface: vars['--fill'],
    surface2: vars['--fill-2'],
    surface3: vars['--fill-3'],
    line: `oklch(${ink} / 0.07)`,
    lineStrong: `oklch(${ink} / 0.14)`,
    text: vars['--text'],
    textDim: vars['--text-dim'],
    textMute: vars['--text-mute'],
    shadow: dark ? '0 16px 40px -16px oklch(0 0 0 / 0.62)' : '0 16px 40px -16px oklch(0 0 0 / 0.28)',
  };
}

export function buildTheme(opts: { dark?: boolean; hue?: number; vibrancy?: number } = {}): Theme {
  const dark = opts.dark ?? true;
  const hue = opts.hue ?? 140;
  const vibrancy = opts.vibrancy ?? 100;
  return {
    dark,
    hue,
    accent: buildAccent(hue, dark, vibrancy),
    n: buildNeutrals(dark, hue),
    r: {
      xs: 8,
      sm: 12,
      md: 16,
      lg: 20,
      xl: 28,
    },
    ok: 'oklch(0.78 0.14 150)',
    warn: 'oklch(0.80 0.14 70)',
    err: 'oklch(0.70 0.18 25)',
    info: 'oklch(0.78 0.10 240)',
    font: {
      sans: '"Manrope", ui-sans-serif, system-ui, -apple-system, sans-serif',
      mono: '"Geist Mono", ui-monospace, SFMono-Regular, Menlo, monospace',
    },
  };
}

export function hashStr(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}
