import type { Theme } from '../tokens';

export interface TileHueVerdict {
  harmonious: boolean;
  message: string | null;
  suggestions: number[];
}

const BLEND_LIMIT = 25;
const ANALOGOUS_LIMIT = 80;
const COMPLEMENT_START = 150;
const PALETTE_SIZE = 7;

const HARMONIOUS_SEGMENTS: Array<[number, number]> = [
  [-180, -COMPLEMENT_START],
  [-ANALOGOUS_LIMIT, -BLEND_LIMIT],
  [BLEND_LIMIT, ANALOGOUS_LIMIT],
  [COMPLEMENT_START, 180],
];

const DOMAIN_LENGTH = HARMONIOUS_SEGMENTS.reduce((sum, [from, to]) => sum + (to - from), 0);

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

const SWAMP_START = 95;
const SWAMP_END = 165;

export function isHueHarmonious(hue: number, theme: Theme): boolean {
  const wrapped = wrapHue(hue);
  const distance = hueDistance(wrapped, theme.hue);
  if (distance < BLEND_LIMIT) return false;
  if (wrapped >= SWAMP_START && wrapped < SWAMP_END && distance > ANALOGOUS_LIMIT) return false;
  return distance <= ANALOGOUS_LIMIT || distance >= COMPLEMENT_START;
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

function offsetAt(position: number): number {
  let remaining = ((position % DOMAIN_LENGTH) + DOMAIN_LENGTH) % DOMAIN_LENGTH;
  for (const [from, to] of HARMONIOUS_SEGMENTS) {
    const length = to - from;
    if (remaining < length) return from + remaining;
    remaining -= length;
  }
  return HARMONIOUS_SEGMENTS[0]![0];
}

export function harmoniousTilePalette(theme: Theme, seed: number): number[] {
  let state = mixSeed(seed);
  const picks: Array<{ position: number; hue: number }> = [];
  let minGap = 22;
  let attempts = 0;
  while (picks.length < PALETTE_SIZE) {
    state = nextRandom(state);
    attempts += 1;
    if (attempts % 24 === 0) minGap = Math.max(6, minGap - 6);
    const position = (state / 4294967296) * DOMAIN_LENGTH;
    const hue = wrapHue(theme.hue + offsetAt(position));
    if (!isHueHarmonious(hue, theme)) continue;
    if (picks.some((pick) => hueDistance(pick.hue, hue) < minGap)) continue;
    picks.push({ position, hue });
  }
  return picks.sort((a, b) => a.position - b.position).map((pick) => pick.hue);
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
