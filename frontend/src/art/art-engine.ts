import { hashStr } from '../tokens';
import type { ArtAspect, ArtPreset } from './InstanceArt';

interface Rgb {
  r: number;
  g: number;
  b: number;
}

interface ColorStop {
  at: number;
  color: Rgb;
}

interface Palette {
  stops: ColorStop[];
  glow: Rgb;
  shade: Rgb;
  mark: Rgb;
}

interface FlowMotif {
  base: number;
  amplitude: number;
  frequency: number;
  phase: number;
  seed: number;
  width: number;
  strength: number;
  slope: number;
}

interface RenderInput {
  seed: number;
  preset: ArtPreset;
  aspect: ArtAspect;
  dark: boolean;
}

interface RenderSize {
  width: number;
  height: number;
}

const CACHE_LIMIT = 64;
const cache = new Map<string, HTMLCanvasElement>();

const SIZE_BY_ASPECT: Record<ArtAspect, RenderSize> = {
  thumb: { width: 128, height: 128 },
  square: { width: 512, height: 512 },
  banner: { width: 1024, height: 486 },
};

function clamp(value: number, min = 0, max = 1): number {
  return Math.max(min, Math.min(max, value));
}

function lerp(a: number, b: number, t: number): number {
  return a + (b - a) * t;
}

function smoothstep(t: number): number {
  return t * t * (3 - 2 * t);
}

function fract(value: number): number {
  return value - Math.floor(value);
}

function mixColor(a: Rgb, b: Rgb, t: number): Rgb {
  return {
    r: lerp(a.r, b.r, t),
    g: lerp(a.g, b.g, t),
    b: lerp(a.b, b.b, t),
  };
}

function clampHue(hue: number): number {
  return ((hue % 360) + 360) % 360;
}

function srgbTransfer(value: number): number {
  return value <= 0.0031308 ? 12.92 * value : 1.055 * value ** (1 / 2.4) - 0.055;
}

function oklch(l: number, c: number, h: number): Rgb {
  const hue = clampHue(h) * Math.PI / 180;
  const a = Math.cos(hue) * c;
  const b = Math.sin(hue) * c;

  const lPrime = l + 0.3963377774 * a + 0.2158037573 * b;
  const mPrime = l - 0.1055613458 * a - 0.0638541728 * b;
  const sPrime = l - 0.0894841775 * a - 1.2914855480 * b;

  const l3 = lPrime ** 3;
  const m3 = mPrime ** 3;
  const s3 = sPrime ** 3;

  return {
    r: Math.round(clamp(srgbTransfer(4.0767416621 * l3 - 3.3077115913 * m3 + 0.2309699292 * s3)) * 255),
    g: Math.round(clamp(srgbTransfer(-1.2684380046 * l3 + 2.6097574011 * m3 - 0.3413193965 * s3)) * 255),
    b: Math.round(clamp(srgbTransfer(-0.0041960863 * l3 - 0.7034186147 * m3 + 1.7076147010 * s3)) * 255),
  };
}

function rng(seed: number): () => number {
  let state = seed >>> 0 || 1;
  return () => {
    state += 0x6d2b79f5;
    let value = state;
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
  };
}

function hash2(x: number, y: number, seed: number): number {
  let h = seed ^ Math.imul(x, 374761393) ^ Math.imul(y, 668265263);
  h = Math.imul(h ^ (h >>> 13), 1274126177);
  return ((h ^ (h >>> 16)) >>> 0) / 4294967296;
}

function valueNoise(x: number, y: number, seed: number): number {
  const ix = Math.floor(x);
  const iy = Math.floor(y);
  const fx = smoothstep(fract(x));
  const fy = smoothstep(fract(y));
  const a = hash2(ix, iy, seed);
  const b = hash2(ix + 1, iy, seed);
  const c = hash2(ix, iy + 1, seed);
  const d = hash2(ix + 1, iy + 1, seed);
  return lerp(lerp(a, b, fx), lerp(c, d, fx), fy);
}

function fbm(x: number, y: number, seed: number, octaves: number): number {
  let value = 0;
  let amplitude = 0.5;
  let frequency = 1;
  let total = 0;
  for (let i = 0; i < octaves; i += 1) {
    value += valueNoise(x * frequency, y * frequency, seed + i * 1013) * amplitude;
    total += amplitude;
    amplitude *= 0.52;
    frequency *= 2.04;
  }
  return value / total;
}

function ridged(x: number, y: number, seed: number): number {
  const n = fbm(x, y, seed, 5);
  return 1 - Math.abs(n * 2 - 1);
}

function paletteColor(stops: ColorStop[], value: number): Rgb {
  const v = clamp(value);
  for (let i = 0; i < stops.length - 1; i += 1) {
    const left = stops[i];
    const right = stops[i + 1];
    if (v <= right.at) {
      return mixColor(left.color, right.color, smoothstep((v - left.at) / (right.at - left.at)));
    }
  }
  return stops[stops.length - 1].color;
}

function softBand(distance: number, width: number): number {
  const d = distance / Math.max(0.0001, width);
  return Math.exp(-d * d);
}

function buildMotifs(rand: () => number, preset: ArtPreset, aspect: ArtAspect): FlowMotif[] {
  const count = aspect === 'thumb' ? 2 : preset === 'mineral' ? 5 : 4;
  return Array.from({ length: count }, (_, index) => ({
    base: 0.12 + index * (0.74 / Math.max(1, count - 1)) + (rand() - 0.5) * 0.10,
    amplitude: (preset === 'silk' ? 0.055 : 0.035) + rand() * 0.065,
    frequency: 1.1 + rand() * (preset === 'ember' ? 2.4 : 1.6),
    phase: rand() * Math.PI * 2,
    seed: Math.floor(rand() * 0xffffffff),
    width: (aspect === 'thumb' ? 0.035 : 0.018) + rand() * (preset === 'mineral' ? 0.012 : 0.022),
    strength: 0.08 + rand() * (preset === 'ember' ? 0.18 : 0.12),
    slope: (rand() - 0.5) * (aspect === 'banner' ? 0.36 : 0.24),
  }));
}

function integratedMotif(
  x: number,
  y: number,
  wx: number,
  wy: number,
  field: number,
  ridge: number,
  warpA: number,
  warpB: number,
  preset: ArtPreset,
  motifs: FlowMotif[],
): { lift: number; shade: number; line: number } {
  let lift = 0;
  let shade = 0;
  let line = 0;

  for (const motif of motifs) {
    const flowY = motif.base
      + motif.slope * (x - 0.5)
      + Math.sin((wx * motif.frequency + motif.phase + warpB * 1.8) * Math.PI * 2) * motif.amplitude
      + (valueNoise(wx * 3.2 + motif.phase, wy * 3.2 - motif.phase, motif.seed) - 0.5) * motif.amplitude * 0.9;
    const band = softBand(y - flowY, motif.width);
    lift += band * motif.strength;
    shade += softBand(y - flowY - motif.width * 1.9, motif.width * 1.8) * motif.strength * 0.38;
    line += softBand(y - flowY, motif.width * 0.32) * motif.strength * 0.32;
  }

  if (preset === 'mineral') {
    const vein = softBand(fract(field * 6.8 + ridge * 0.55 + warpA * 0.25) - 0.5, 0.030);
    const seam = softBand(fract((wx * 0.82 + wy * 1.38 + warpB * 0.22) * 5.0) - 0.5, 0.026);
    line += vein * 0.11 + seam * 0.07;
    shade += seam * 0.05;
  } else if (preset === 'ember') {
    const heat = softBand(fract((ridge + field * 0.72 + warpA * 0.20) * 4.4) - 0.5, 0.040);
    lift += heat * 0.14;
    line += heat * 0.06;
  } else if (preset === 'silk') {
    const fold = softBand(Math.sin((wx * 7.2 + wy * 2.1 + warpA * 2.4) * Math.PI) * 0.5, 0.22);
    lift += fold * 0.05;
    shade += (1 - fold) * 0.025;
  } else {
    const aurora = softBand(Math.sin((wx * 4.6 - wy * 1.3 + warpA * 1.7) * Math.PI) * 0.5, 0.18);
    lift += aurora * 0.08;
    line += aurora * 0.035;
  }

  return {
    lift: clamp(lift, 0, 0.42),
    shade: clamp(shade, 0, 0.22),
    line: clamp(line, 0, 0.18),
  };
}

function paletteFor(seed: number, preset: ArtPreset, dark: boolean): Palette {
  const base = seed % 360;
  const profile = {
    aurora: { offsets: [172, 128, 72, 18], chroma: 0.12 },
    silk: { offsets: [24, 58, 112, 158], chroma: 0.095 },
    mineral: { offsets: [206, 244, 296, 332], chroma: 0.075 },
    ember: { offsets: [350, 20, 48, 78], chroma: 0.13 },
  }[preset];
  const lows = dark ? [0.16, 0.24, 0.36, 0.57] : [0.66, 0.76, 0.86, 0.94];
  const chroma = profile.chroma;
  return {
    stops: [
      { at: 0, color: oklch(lows[0], chroma * 0.50, base + profile.offsets[0]) },
      { at: 0.34, color: oklch(lows[1], chroma * 0.82, base + profile.offsets[1]) },
      { at: 0.68, color: oklch(lows[2], chroma * 0.95, base + profile.offsets[2]) },
      { at: 1, color: oklch(lows[3], chroma * 0.62, base + profile.offsets[3]) },
    ],
    glow: oklch(dark ? 0.78 : 0.58, chroma + 0.04, base + profile.offsets[2]),
    shade: oklch(dark ? 0.08 : 0.34, 0.018, base + profile.offsets[0]),
    mark: oklch(dark ? 0.70 : 0.46, chroma * 0.58, base + profile.offsets[1]),
  };
}

function sizeFor(aspect: ArtAspect): RenderSize {
  return SIZE_BY_ASPECT[aspect];
}

function cacheKey(input: RenderInput): string {
  const { width, height } = sizeFor(input.aspect);
  return `${input.seed}:${input.preset}:${input.aspect}:${input.dark ? 'd' : 'l'}:${width}x${height}`;
}

function enforceCacheLimit(): void {
  while (cache.size > CACHE_LIMIT) {
    const oldest = cache.keys().next().value;
    if (oldest == null) return;
    cache.delete(oldest);
  }
}

function lightMask(x: number, y: number, lx: number, ly: number): number {
  const dx = x - lx;
  const dy = y - ly;
  return Math.exp(-(dx * dx * 2.5 + dy * dy * 4.2));
}

function renderPixels(input: RenderInput, target: HTMLCanvasElement): void {
  const { width, height } = sizeFor(input.aspect);
  const ctx = target.getContext('2d');
  if (!ctx) return;

  target.width = width;
  target.height = height;

  const rand = rng(input.seed ^ hashStr(`${input.preset}:${input.aspect}:${input.dark ? 'dark' : 'light'}`));
  const palette = paletteFor(input.seed, input.preset, input.dark);
  const image = ctx.createImageData(width, height);
  const data = image.data;
  const stretch = width / height;
  const fieldSeed = input.seed ^ 0x9e3779b9;
  const warpSeed = input.seed ^ 0x85ebca6b;
  const detailSeed = input.seed ^ 0xc2b2ae35;
  const light = { x: 0.16 + rand() * 0.68, y: 0.12 + rand() * 0.62 };
  const fillLight = { x: 0.18 + rand() * 0.64, y: 0.10 + rand() * 0.58 };
  const diagonal = rand() > 0.5 ? 1 : -1;
  const beamOffset = 0.20 + rand() * 0.18;
  const strataAngle = (rand() * 0.9 - 0.45) + (input.aspect === 'banner' ? 0.16 : -0.04);
  const motifs = buildMotifs(rand, input.preset, input.aspect);

  for (let py = 0; py < height; py += 1) {
    const y = py / Math.max(1, height - 1);
    for (let px = 0; px < width; px += 1) {
      const x = px / Math.max(1, width - 1);
      const ux = (x - 0.5) * stretch;
      const uy = y - 0.5;
      const warpA = fbm(x * 2.2 + 13.1, y * 2.2 - 7.4, warpSeed, 4) - 0.5;
      const warpB = fbm(x * 2.0 - 2.8, y * 2.0 + 17.6, warpSeed + 37, 4) - 0.5;
      const wx = x + warpA * 0.22;
      const wy = y + warpB * 0.18;

      let field = fbm(wx * 2.8 + diagonal * wy * 0.9, wy * 2.5 - diagonal * wx * 0.35, fieldSeed, 5);
      const ridge = ridged(wx * 5.5 + wy * 1.4, wy * 5.1, detailSeed);
      const strata = Math.sin((wx * Math.cos(strataAngle) + wy * Math.sin(strataAngle)) * Math.PI * 8 + warpA * 4);
      const beam = clamp(1 - Math.abs((x - y * 0.72 * diagonal) - beamOffset) * 2.5);
      const glow = Math.exp(-(((x - light.x) ** 2) * 3.1 + ((y - light.y) ** 2) * 4.6));
      const vignette = clamp(1 - Math.sqrt(ux * ux + uy * uy) * 0.92);

      if (input.preset === 'mineral') {
        field = field * 0.52 + ridge * 0.30 + (strata * 0.5 + 0.5) * 0.18;
      } else if (input.preset === 'silk') {
        field = field * 0.62 + (Math.sin((wx + warpB * 0.35) * 14 + wy * 5) * 0.5 + 0.5) * 0.20 + glow * 0.18;
      } else if (input.preset === 'ember') {
        field = field * 0.50 + ridge * 0.18 + glow * 0.32;
      } else {
        field = field * 0.56 + beam * 0.18 + glow * 0.26;
      }

      field = clamp(field * (0.82 + vignette * 0.26) + (input.dark ? -0.03 : 0.015));
      const motif = integratedMotif(x, y, wx, wy, field, ridge, warpA, warpB, input.preset, motifs);
      field = clamp(field + motif.lift * 0.18 - motif.shade * 0.12);
      let color = paletteColor(palette.stops, field);

      const illuminate = clamp(glow * 0.48 + beam * 0.15 + motif.lift * 0.30 + lightMask(x, y, fillLight.x, fillLight.y) * 0.04);
      color = mixColor(color, palette.glow, illuminate);
      color = mixColor(color, palette.shade, clamp((1 - vignette) * (input.dark ? 0.48 : 0.20) + motif.shade));

      const contour = input.preset === 'mineral'
        ? clamp(1 - Math.abs(fract(field * 8.0 + ridge * 0.35) - 0.5) * 24)
        : clamp(1 - Math.abs(fract(field * 5.0 + warpA * 0.15) - 0.5) * 18);
      color = mixColor(color, palette.mark, contour * (input.aspect === 'thumb' ? 0.025 : 0.040) + motif.line);

      const grain = hash2(px, py, detailSeed) - 0.5;
      const grainAmount = input.aspect === 'banner' ? 7 : 5;
      const index = (py * width + px) * 4;
      data[index] = clamp(Math.round(color.r + grain * grainAmount), 0, 255);
      data[index + 1] = clamp(Math.round(color.g + grain * grainAmount), 0, 255);
      data[index + 2] = clamp(Math.round(color.b + grain * grainAmount), 0, 255);
      data[index + 3] = 255;
    }
  }

  ctx.putImageData(image, 0, 0);
}

export function renderInstanceArt(input: RenderInput): HTMLCanvasElement {
  const key = cacheKey(input);
  const cached = cache.get(key);
  if (cached) {
    cache.delete(key);
    cache.set(key, cached);
    return cached;
  }

  const canvas = document.createElement('canvas');
  renderPixels(input, canvas);
  cache.set(key, canvas);
  enforceCacheLimit();
  return canvas;
}
