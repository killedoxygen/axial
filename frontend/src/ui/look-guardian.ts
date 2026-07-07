import type { Theme } from '../tokens';

export interface TileHueVerdict {
  harmonious: boolean;
  message: string | null;
  suggestions: number[];
}

const BLEND_LIMIT = 25;
const ANALOGOUS_LIMIT = 80;
const PALETTE_SIZE = 7;

const HARMONIOUS_SEGMENTS: Array<[number, number]> = [
  [-ANALOGOUS_LIMIT, -BLEND_LIMIT],
  [BLEND_LIMIT, ANALOGOUS_LIMIT],
];

const PALETTE_PROFILES: Array<Array<[number, number]>> = [
  [[-80, -32]],
  [[32, 80]],
  [
    [-72, -42],
    [42, 72],
  ],
  [
    [-58, -25],
    [58, 80],
  ],
  [
    [-80, -58],
    [25, 55],
  ],
  [
    [-55, -25],
    [50, 80],
  ],
  HARMONIOUS_SEGMENTS,
];

export function wrapHue(hue: number): number {
  return ((Math.round(hue) % 360) + 360) % 360;
}

export function hueDistance(a: number, b: number): number {
  const delta = Math.abs(wrapHue(a) - wrapHue(b));
  return Math.min(delta, 360 - delta);
}

export function seedForTileHue(hue: number): number {
  const wrapped = wrapHue(hue);
  return wrapped === 0 ? 360 : wrapped;
}

export function isHueHarmonious(hue: number, theme: Theme): boolean {
  const wrapped = wrapHue(hue);
  const distance = hueDistance(wrapped, theme.hue);
  return distance >= BLEND_LIMIT && distance <= ANALOGOUS_LIMIT;
}

function nextRandom(state: number): number {
  return Math.imul(state ^ 0x9e3779b9, 2654435761) >>> 0 || 1;
}

function mixSeed(seed: number): number {
  let mixed = seed >>> 0;
  mixed ^= mixed >>> 16;
  mixed = Math.imul(mixed, 0x85ebca6b) >>> 0;
  mixed ^= mixed >>> 13;
  mixed = Math.imul(mixed, 0xc2b2ae35) >>> 0;
  mixed ^= mixed >>> 16;
  return mixed || 1;
}

function domainLength(segments: Array<[number, number]>): number {
  return segments.reduce((sum, [from, to]) => sum + (to - from), 0);
}

function offsetAt(position: number, segments: Array<[number, number]>): number {
  const lengthTotal = domainLength(segments);
  let remaining = ((position % lengthTotal) + lengthTotal) % lengthTotal;
  for (const [from, to] of segments) {
    const length = to - from;
    if (remaining < length) return from + remaining;
    remaining -= length;
  }
  return segments[0]![0];
}

function paletteProfile(seed: number): Array<[number, number]> {
  return PALETTE_PROFILES[((seed >>> 24) + (seed >>> 12)) % PALETTE_PROFILES.length]!;
}

function evenlySpreadPalette(theme: Theme, seed: number, segments: Array<[number, number]>): number[] {
  let state = seed;
  const lengthTotal = domainLength(segments);
  const step = lengthTotal / PALETTE_SIZE;
  const colors: Array<{ position: number; hue: number }> = [];

  state = nextRandom(state);
  const phase = (state / 4294967296) * step;
  const jitterLimit = Math.min(5, step * 0.4);

  for (let index = 0; index < PALETTE_SIZE; index += 1) {
    state = nextRandom(state);
    const jitter = (state / 4294967296 - 0.5) * jitterLimit * 2;
    const position = phase + (index + 0.5) * step + jitter;
    colors.push({
      position,
      hue: wrapHue(theme.hue + offsetAt(position, segments)),
    });
  }

  return colors.sort((a, b) => a.position - b.position).map((color) => color.hue);
}

export function harmoniousTilePalette(theme: Theme, seed: number): number[] {
  let state = mixSeed(seed);
  const segments = paletteProfile(state);
  const lengthTotal = domainLength(segments);
  const picks: Array<{ position: number; hue: number }> = [];
  let minGap = 22;
  let attempts = 0;
  while (picks.length < PALETTE_SIZE && attempts < 240) {
    state = nextRandom(state);
    attempts += 1;
    if (attempts % 24 === 0) minGap = Math.max(6, minGap - 6);
    const position = (state / 4294967296) * lengthTotal;
    const hue = wrapHue(theme.hue + offsetAt(position, segments));
    if (!isHueHarmonious(hue, theme)) continue;
    if (picks.some((pick) => hueDistance(pick.hue, hue) < minGap)) continue;
    picks.push({ position, hue });
  }
  if (picks.length < PALETTE_SIZE) return evenlySpreadPalette(theme, state, segments);
  return picks.sort((a, b) => a.position - b.position).map((pick) => pick.hue);
}

export function resolveTileHue(hue: number, theme: Theme): number {
  const wrapped = wrapHue(hue);
  if (isHueHarmonious(wrapped, theme)) return wrapped;
  return [...harmoniousTilePalette(theme, seedForTileHue(wrapped))].sort(
    (a, b) => hueDistance(a, wrapped) - hueDistance(b, wrapped),
  )[0]!;
}

export function assessTileHue(hue: number, theme: Theme): TileHueVerdict {
  if (isHueHarmonious(hue, theme)) {
    return { harmonious: true, message: null, suggestions: [] };
  }
  const suggestions = [...harmoniousTilePalette(theme, seedForTileHue(hue))]
    .sort((a, b) => hueDistance(a, hue) - hueDistance(b, hue))
    .slice(0, 2);
  return {
    harmonious: false,
    message: 'This color will not sit well with your theme',
    suggestions,
  };
}
