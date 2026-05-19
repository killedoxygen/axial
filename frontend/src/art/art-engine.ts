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

export type VersionLifecycleTrait = 'snapshot' | 'experimental' | 'old_beta' | 'old_alpha' | 'pre_release' | 'release_candidate';
export type VersionLoaderTrait = 'fabric' | 'quilt' | 'forge' | 'neoforge';

export interface VersionIdentity {
  label: string;
  lifecycleTrait?: VersionLifecycleTrait | null;
  loaderTrait?: VersionLoaderTrait | null;
}

interface RenderInput {
  seed: number;
  preset: ArtPreset;
  aspect: ArtAspect;
  dark: boolean;
  renderSize?: RenderSize | null;
  versionIdentity?: VersionIdentity | null;
}

interface RenderSize {
  width: number;
  height: number;
}

interface RenderDetail {
  fieldOctaves: number;
  warpOctaves: number;
  ridgeOctaves: number;
  plumeOctaves: number;
  orbitIterations: number;
  motifScale: number;
}

const MAX_CACHE_BYTES = 32 * 1024 * 1024;
interface RenderedArt {
  source: HTMLCanvasElement;
  width: number;
  height: number;
}

const cache = new Map<string, RenderedArt>();
const cacheSizes = new Map<string, number>();
let cacheBytes = 0;

const SIZE_BY_ASPECT: Record<ArtAspect, RenderSize> = {
  thumb: { width: 128, height: 128 },
  square: { width: 512, height: 512 },
  banner: { width: 1024, height: 486 },
};

const MAX_PIXELS_BY_ASPECT: Record<ArtAspect, number> = {
  thumb: 128 * 128,
  square: 320 * 320,
  banner: 800 * 240,
};

function clamp(value: number, min = 0, max = 1): number {
  return Math.max(min, Math.min(max, value));
}

function clampByte(value: number): number {
  return Math.max(0, Math.min(255, Math.round(value)));
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
  const channel = Math.max(0, value);
  return channel <= 0.0031308 ? 12.92 * channel : 1.055 * channel ** (1 / 2.4) - 0.055;
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

function ridged(x: number, y: number, seed: number, octaves: number): number {
  const n = fbm(x, y, seed, octaves);
  return 1 - Math.abs(n * 2 - 1);
}

function fractalOrbit(x: number, y: number, seed: number, iterations: number): number {
  const angle = (seed % 6283) / 1000;
  const cx = Math.cos(angle) * 0.42 - 0.18;
  const cy = Math.sin(angle * 1.37) * 0.34;
  let zx = x * 2.15 - 1.08;
  let zy = y * 2.15 - 1.08;
  let trap = 10;
  let escape = 0;

  for (let i = 0; i < iterations; i += 1) {
    const xx = zx * zx - zy * zy + cx;
    const yy = 2 * zx * zy + cy;
    zx = xx;
    zy = yy;
    const radius = Math.sqrt(zx * zx + zy * zy);
    trap = Math.min(trap, Math.abs(radius - 0.72) + Math.abs(zx + zy) * 0.045);
    if (radius > 2.6 && escape === 0) {
      escape = i / iterations;
      break;
    }
  }

  return clamp((1 - trap * 2.5) * 0.72 + escape * 0.28);
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

function buildMotifs(rand: () => number, preset: ArtPreset, aspect: ArtAspect, scale = 1): FlowMotif[] {
  const baseCount = aspect === 'thumb'
    ? 2
    : preset === 'mineral' || preset === 'topo' || preset === 'dune'
      ? 5
      : preset === 'vapor'
        ? 6
        : 4;
  const count = Math.max(1, Math.round(baseCount * scale));
  const wideFlow = preset === 'vapor' || preset === 'dune';
  const tightFlow = preset === 'topo' || preset === 'prism' || preset === 'orbit';
  return Array.from({ length: count }, (_, index) => ({
    base: 0.12 + index * (0.74 / Math.max(1, count - 1)) + (rand() - 0.5) * 0.10,
    amplitude: (preset === 'silk' || wideFlow ? 0.055 : 0.030) + rand() * (tightFlow ? 0.035 : 0.065),
    frequency: 1.1 + rand() * (preset === 'ember' || preset === 'prism' || preset === 'orbit' ? 2.4 : preset === 'vapor' ? 1.0 : 1.6),
    phase: rand() * Math.PI * 2,
    seed: Math.floor(rand() * 0xffffffff),
    width: (aspect === 'thumb' ? 0.035 : wideFlow ? 0.030 : 0.016) + rand() * (tightFlow ? 0.010 : 0.024),
    strength: 0.08 + rand() * (preset === 'ember' || preset === 'vapor' ? 0.18 : 0.12),
    slope: (rand() - 0.5) * (aspect === 'banner' ? preset === 'dune' ? 0.22 : 0.36 : 0.24),
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
  detail: RenderDetail,
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
  } else if (preset === 'topo') {
    const contour = softBand(fract(field * 9.5 + ridge * 0.36 + warpA * 0.24) - 0.5, 0.034);
    const shelf = softBand(fract((wy + warpB * 0.18 + field * 0.26) * 7.0) - 0.5, 0.038);
    line += contour * 0.12 + shelf * 0.055;
    shade += shelf * 0.07;
  } else if (preset === 'prism') {
    const facetA = softBand(Math.sin((wx * 5.2 + wy * 3.1 + warpA * 1.8) * Math.PI) * 0.5, 0.16);
    const facetB = softBand(Math.sin((wx * -3.8 + wy * 5.7 + warpB * 2.0) * Math.PI) * 0.5, 0.14);
    lift += facetA * 0.08;
    shade += facetB * 0.06;
    line += Math.min(facetA, facetB) * 0.08;
  } else if (preset === 'vapor') {
    const plume = fbm(wx * 3.4 + field * 0.6, wy * 4.2 + warpA * 0.8, motifs[1]?.seed ?? 1, detail.plumeOctaves);
    const veil = softBand(plume - 0.56, 0.20);
    lift += veil * 0.18;
    shade += (1 - veil) * 0.035;
    line += veil * 0.035;
  } else if (preset === 'dune') {
    const ripple = softBand(Math.sin((wy * 13.0 + wx * 2.2 + warpA * 2.0) * Math.PI) * 0.5, 0.20);
    const slip = softBand(fract((wy + field * 0.24 + warpB * 0.16) * 8.0) - 0.5, 0.040);
    lift += ripple * 0.075;
    shade += slip * 0.075;
    line += slip * 0.045;
  } else if (preset === 'orbit') {
    const orbit = fractalOrbit(wx + warpA * 0.24, wy + warpB * 0.24, motifs[0]?.seed ?? 1, detail.orbitIterations);
    const ring = softBand(orbit - 0.62, 0.18);
    lift += orbit * 0.13;
    shade += (1 - orbit) * 0.04;
    line += ring * 0.075;
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
    vapor: { offsets: [188, 222, 282, 326], chroma: 0.105 },
    topo: { offsets: [118, 152, 196, 62], chroma: 0.082 },
    prism: { offsets: [268, 324, 36, 184], chroma: 0.118 },
    dune: { offsets: [54, 78, 112, 28], chroma: 0.090 },
    orbit: { offsets: [224, 274, 318, 34], chroma: 0.110 },
  }[preset];
  const lows = preset === 'dune'
    ? dark ? [0.18, 0.27, 0.41, 0.62] : [0.70, 0.79, 0.88, 0.95]
    : preset === 'prism' || preset === 'orbit'
      ? dark ? [0.14, 0.24, 0.40, 0.64] : [0.64, 0.75, 0.87, 0.96]
      : dark ? [0.16, 0.24, 0.36, 0.57] : [0.66, 0.76, 0.86, 0.94];
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

function sizeFor(input: Pick<RenderInput, 'aspect' | 'renderSize'>): RenderSize {
  const max = SIZE_BY_ASPECT[input.aspect];
  const requested = input.renderSize;
  if (!requested) return max;
  let width = Math.max(1, Math.min(max.width, Math.round(requested.width)));
  let height = Math.max(1, Math.min(max.height, Math.round(requested.height)));
  const pixels = width * height;
  const pixelBudget = MAX_PIXELS_BY_ASPECT[input.aspect];
  if (pixels > pixelBudget) {
    const scale = Math.sqrt(pixelBudget / pixels);
    width = Math.max(1, Math.round(width * scale));
    height = Math.max(1, Math.round(height * scale));
  }
  return { width, height };
}

function cacheKey(input: RenderInput): string {
  const { width, height } = sizeFor(input);
  const identity = input.versionIdentity;
  const lifecycle = identity?.lifecycleTrait ?? '';
  const loader = identity?.loaderTrait ?? '';
  return `${input.seed}:${input.preset}:${input.aspect}:${input.dark ? 'd' : 'l'}:${width}x${height}:${lifecycle}:${loader}`;
}

function cacheByteSize(input: RenderInput): number {
  const { width, height } = sizeFor(input);
  return width * height * 4;
}

function detailFor(input: RenderInput, width: number, height: number): RenderDetail {
  const pixels = width * height;
  if (input.aspect === 'thumb' || pixels <= 24_000) {
    return { fieldOctaves: 3, warpOctaves: 2, ridgeOctaves: 3, plumeOctaves: 2, orbitIterations: 5, motifScale: 0.55 };
  }
  if (pixels <= 90_000) {
    return { fieldOctaves: 4, warpOctaves: 3, ridgeOctaves: 3, plumeOctaves: 3, orbitIterations: 6, motifScale: 0.75 };
  }
  if (pixels <= 180_000) {
    return { fieldOctaves: 4, warpOctaves: 3, ridgeOctaves: 4, plumeOctaves: 3, orbitIterations: 8, motifScale: 0.90 };
  }
  return { fieldOctaves: 5, warpOctaves: 4, ridgeOctaves: 5, plumeOctaves: 4, orbitIterations: 10, motifScale: 1 };
}

function deleteCached(key: string): void {
  cache.delete(key);
  cacheBytes -= cacheSizes.get(key) ?? 0;
  cacheSizes.delete(key);
  cacheBytes = Math.max(0, cacheBytes);
}

function enforceCacheLimit(): void {
  while (cacheBytes > MAX_CACHE_BYTES) {
    const oldest = cache.keys().next().value;
    if (oldest == null) return;
    deleteCached(oldest);
  }
}

function lightMask(x: number, y: number, lx: number, ly: number): number {
  const dx = x - lx;
  const dy = y - ly;
  return Math.exp(-(dx * dx * 2.5 + dy * dy * 4.2));
}

function rgbCss(color: Rgb, alpha = 1): string {
  return `rgba(${Math.round(color.r)}, ${Math.round(color.g)}, ${Math.round(color.b)}, ${alpha})`;
}

function drawPhaseSlices(
  ctx: CanvasRenderingContext2D,
  width: number,
  height: number,
  palette: Palette,
): void {
  ctx.save();
  ctx.beginPath();
  ctx.rect(width * 0.12, height * 0.29, width * 0.76, height * 0.42);
  ctx.clip();
  ctx.globalCompositeOperation = 'screen';
  ctx.strokeStyle = rgbCss(palette.glow, 0.24);
  ctx.lineWidth = Math.max(3, width * 0.012);
  ctx.beginPath();
  ctx.moveTo(width * 0.12, height * 0.63);
  ctx.lineTo(width * 0.88, height * 0.35);
  ctx.moveTo(width * 0.17, height * 0.70);
  ctx.lineTo(width * 0.83, height * 0.42);
  ctx.stroke();
  ctx.restore();
}

function drawExperimentalOrbit(
  ctx: CanvasRenderingContext2D,
  width: number,
  height: number,
  palette: Palette,
): void {
  ctx.save();
  ctx.globalCompositeOperation = 'screen';
  ctx.strokeStyle = rgbCss(palette.glow, 0.22);
  ctx.lineWidth = Math.max(2, width * 0.007);
  ctx.beginPath();
  ctx.ellipse(width * 0.50, height * 0.50, width * 0.31, height * 0.18, -0.32, 0.20, Math.PI * 1.46);
  ctx.stroke();
  ctx.beginPath();
  ctx.ellipse(width * 0.50, height * 0.50, width * 0.31, height * 0.18, -0.32, Math.PI * 1.63, Math.PI * 1.92);
  ctx.stroke();
  ctx.restore();
}

function drawLegacySlab(
  ctx: CanvasRenderingContext2D,
  width: number,
  height: number,
  palette: Palette,
  rough: boolean,
): void {
  const x = width * 0.16;
  const y = height * 0.33;
  const w = width * 0.68;
  const h = height * 0.34;
  const cut = width * 0.055;
  ctx.save();
  ctx.globalCompositeOperation = 'multiply';
  ctx.fillStyle = rgbCss(palette.shade, 0.28);
  ctx.beginPath();
  ctx.moveTo(x + cut, y);
  ctx.lineTo(x + w, y);
  ctx.lineTo(x + w - cut, y + h);
  ctx.lineTo(x, y + h);
  ctx.closePath();
  ctx.fill();

  ctx.globalCompositeOperation = 'screen';
  ctx.fillStyle = rgbCss(palette.glow, 0.10);
  ctx.fillRect(x + w * 0.10, y + h * 0.18, w * 0.78, Math.max(2, h * 0.035));
  if (rough) {
    ctx.fillRect(x + w * 0.13, y - 1, cut * 0.70, cut * 0.38);
    ctx.fillRect(x + w * 0.62, y + h - cut * 0.28, cut * 0.62, cut * 0.32);
    ctx.fillRect(x + w * 0.84, y + h * 0.34, cut * 0.42, cut * 0.28);
  } else {
    ctx.globalCompositeOperation = 'multiply';
    ctx.fillStyle = rgbCss(palette.shade, 0.26);
    ctx.fillRect(x + w * 0.10, y, cut * 0.70, cut * 0.35);
    ctx.fillRect(x + w * 0.77, y + h - cut * 0.34, cut * 0.70, cut * 0.35);
  }
  ctx.restore();
}

function drawProgressRing(
  ctx: CanvasRenderingContext2D,
  width: number,
  height: number,
  palette: Palette,
  candidate: boolean,
): void {
  ctx.save();
  ctx.globalCompositeOperation = 'screen';
  ctx.strokeStyle = rgbCss(palette.glow, 0.25);
  ctx.lineWidth = Math.max(3, width * 0.010);
  ctx.beginPath();
  ctx.arc(width * 0.50, height * 0.50, width * 0.33, -Math.PI * 0.88, candidate ? Math.PI * 0.20 : -Math.PI * 0.02);
  ctx.stroke();
  ctx.fillStyle = rgbCss(palette.glow, 0.28);
  ctx.beginPath();
  ctx.moveTo(width * 0.78, height * 0.35);
  ctx.lineTo(width * 0.82, height * 0.36);
  ctx.lineTo(width * 0.79, height * 0.39);
  ctx.closePath();
  ctx.fill();
  ctx.restore();
}

function drawLoaderTrait(
  ctx: CanvasRenderingContext2D,
  width: number,
  height: number,
  palette: Palette,
  trait: VersionLoaderTrait,
): void {
  ctx.save();
  ctx.globalCompositeOperation = 'screen';
  ctx.strokeStyle = rgbCss(palette.glow, 0.20);
  ctx.fillStyle = rgbCss(palette.glow, 0.15);
  ctx.lineWidth = Math.max(2, width * 0.006);

  if (trait === 'fabric') {
    for (let i = 0; i < 4; i += 1) {
      const y = height * (0.39 + i * 0.072);
      ctx.beginPath();
      ctx.moveTo(width * 0.20, y);
      ctx.bezierCurveTo(width * 0.38, y - 18, width * 0.62, y + 18, width * 0.80, y);
      ctx.stroke();
    }
  } else if (trait === 'quilt') {
    const size = width * 0.038;
    for (let row = 0; row < 3; row += 1) {
      for (let col = 0; col < 3; col += 1) {
        const cx = width * 0.68 + col * size * 1.22;
        const cy = height * 0.32 + row * size * 1.22;
        ctx.beginPath();
        ctx.moveTo(cx, cy - size * 0.48);
        ctx.lineTo(cx + size * 0.48, cy);
        ctx.lineTo(cx, cy + size * 0.48);
        ctx.lineTo(cx - size * 0.48, cy);
        ctx.closePath();
        ctx.fill();
      }
    }
  } else if (trait === 'forge') {
    ctx.globalCompositeOperation = 'multiply';
    ctx.fillStyle = rgbCss(palette.shade, 0.24);
    ctx.fillRect(width * 0.30, height * 0.65, width * 0.40, height * 0.030);
    ctx.fillRect(width * 0.38, height * 0.70, width * 0.24, height * 0.025);
  } else {
    ctx.beginPath();
    ctx.arc(width * 0.50, height * 0.50, width * 0.34, -Math.PI * 0.22, Math.PI * 0.66);
    ctx.stroke();
    ctx.beginPath();
    ctx.arc(width * 0.50, height * 0.50, width * 0.28, Math.PI * 0.86, Math.PI * 1.42);
    ctx.stroke();
  }
  ctx.restore();
}

function drawVersionIdentity(
  ctx: CanvasRenderingContext2D,
  input: RenderInput,
  palette: Palette,
  width: number,
  height: number,
): void {
  if (input.aspect !== 'square') return;
  const identity = input.versionIdentity;
  if (!identity) return;
  const lifecycle = identity.lifecycleTrait ?? null;
  const loader = identity.loaderTrait ?? null;
  if (!lifecycle && !loader) return;

  ctx.save();
  const basin = ctx.createRadialGradient(width * 0.50, height * 0.50, width * 0.08, width * 0.50, height * 0.50, width * 0.48);
  basin.addColorStop(0, rgbCss(input.dark ? palette.shade : palette.glow, input.dark ? 0.30 : 0.20));
  basin.addColorStop(0.58, rgbCss(input.dark ? palette.shade : palette.glow, input.dark ? 0.16 : 0.12));
  basin.addColorStop(1, rgbCss(input.dark ? palette.shade : palette.glow, 0));
  ctx.globalCompositeOperation = input.dark ? 'multiply' : 'screen';
  ctx.fillStyle = basin;
  ctx.fillRect(0, 0, width, height);
  ctx.restore();

  if (lifecycle === 'snapshot' || lifecycle === 'experimental') drawPhaseSlices(ctx, width, height, palette);
  if (lifecycle === 'experimental') drawExperimentalOrbit(ctx, width, height, palette);
  if (lifecycle === 'old_beta' || lifecycle === 'old_alpha') drawLegacySlab(ctx, width, height, palette, lifecycle === 'old_alpha');
  if (lifecycle === 'pre_release' || lifecycle === 'release_candidate') {
    drawProgressRing(ctx, width, height, palette, lifecycle === 'release_candidate');
  }
  if (loader) drawLoaderTrait(ctx, width, height, palette, loader);
}

function renderPixels(input: RenderInput): RenderedArt {
  const { width, height } = sizeFor(input);
  const detail = detailFor(input, width, height);
  const rand = rng(input.seed ^ hashStr(`${input.preset}:${input.aspect}:${input.dark ? 'dark' : 'light'}`));
  const palette = paletteFor(input.seed, input.preset, input.dark);
  const image = new ImageData(width, height);
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
  const strataCos = Math.cos(strataAngle);
  const strataSin = Math.sin(strataAngle);
  const invWidth = 1 / Math.max(1, width - 1);
  const invHeight = 1 / Math.max(1, height - 1);
  const motifs = buildMotifs(rand, input.preset, input.aspect, detail.motifScale);

  for (let py = 0; py < height; py += 1) {
    const y = py * invHeight;
    for (let px = 0; px < width; px += 1) {
      const x = px * invWidth;
      const ux = (x - 0.5) * stretch;
      const uy = y - 0.5;
      const warpA = fbm(x * 2.2 + 13.1, y * 2.2 - 7.4, warpSeed, detail.warpOctaves) - 0.5;
      const warpB = fbm(x * 2.0 - 2.8, y * 2.0 + 17.6, warpSeed + 37, detail.warpOctaves) - 0.5;
      const wx = x + warpA * 0.22;
      const wy = y + warpB * 0.18;

      let field = fbm(wx * 2.8 + diagonal * wy * 0.9, wy * 2.5 - diagonal * wx * 0.35, fieldSeed, detail.fieldOctaves);
      const ridge = ridged(wx * 5.5 + wy * 1.4, wy * 5.1, detailSeed, detail.ridgeOctaves);
      const strata = Math.sin((wx * strataCos + wy * strataSin) * Math.PI * 8 + warpA * 4);
      const beam = clamp(1 - Math.abs((x - y * 0.72 * diagonal) - beamOffset) * 2.5);
      const lightDx = x - light.x;
      const lightDy = y - light.y;
      const glow = Math.exp(-(lightDx * lightDx * 3.1 + lightDy * lightDy * 4.6));
      const vignette = clamp(1 - Math.sqrt(ux * ux + uy * uy) * 0.92);

      if (input.preset === 'mineral') {
        field = field * 0.52 + ridge * 0.30 + (strata * 0.5 + 0.5) * 0.18;
      } else if (input.preset === 'topo') {
        const terrace = Math.floor((field + ridge * 0.34) * 7) / 7;
        field = field * 0.44 + terrace * 0.24 + ridge * 0.18 + (strata * 0.5 + 0.5) * 0.14;
      } else if (input.preset === 'prism') {
        const refraction = Math.sin((wx * 3.4 - wy * 2.7 + warpA * 3.0) * Math.PI) * 0.5 + 0.5;
        const split = Math.sin((wx * -4.2 + wy * 4.8 + warpB * 2.5) * Math.PI) * 0.5 + 0.5;
        field = field * 0.45 + refraction * 0.24 + split * 0.16 + glow * 0.15;
      } else if (input.preset === 'vapor') {
        const plume = fbm(wx * 1.7 + warpB, wy * 2.5 - warpA, detailSeed + 211, detail.plumeOctaves);
        field = field * 0.38 + plume * 0.34 + glow * 0.20 + beam * 0.08;
      } else if (input.preset === 'dune') {
        const sediment = Math.sin((wy * 10.5 + wx * 1.2 + warpA * 1.8) * Math.PI) * 0.5 + 0.5;
        field = field * 0.48 + sediment * 0.24 + ridge * 0.14 + beam * 0.14;
      } else if (input.preset === 'orbit') {
        const orbit = fractalOrbit(wx + warpA * 0.20, wy + warpB * 0.20, detailSeed, detail.orbitIterations);
        const halo = softBand(orbit - 0.58, 0.24);
        field = field * 0.42 + orbit * 0.24 + halo * 0.18 + glow * 0.16;
      } else if (input.preset === 'silk') {
        field = field * 0.62 + (Math.sin((wx + warpB * 0.35) * 14 + wy * 5) * 0.5 + 0.5) * 0.20 + glow * 0.18;
      } else if (input.preset === 'ember') {
        field = field * 0.50 + ridge * 0.18 + glow * 0.32;
      } else {
        field = field * 0.56 + beam * 0.18 + glow * 0.26;
      }

      field = clamp(field * (0.82 + vignette * 0.26) + (input.dark ? -0.03 : 0.015));
      const motif = integratedMotif(x, y, wx, wy, field, ridge, warpA, warpB, input.preset, motifs, detail);
      field = clamp(field + motif.lift * 0.18 - motif.shade * 0.12);
      let color = paletteColor(palette.stops, field);

      const illuminate = clamp(glow * 0.48 + beam * 0.15 + motif.lift * 0.30 + lightMask(x, y, fillLight.x, fillLight.y) * 0.04);
      color = mixColor(color, palette.glow, illuminate);
      color = mixColor(color, palette.shade, clamp((1 - vignette) * (input.dark ? 0.48 : 0.20) + motif.shade));

      const contour = input.preset === 'mineral' || input.preset === 'topo'
        ? clamp(1 - Math.abs(fract(field * (input.preset === 'topo' ? 11.0 : 8.0) + ridge * 0.35) - 0.5) * (input.preset === 'topo' ? 30 : 24))
        : input.preset === 'dune'
          ? clamp(1 - Math.abs(fract((field + wy * 0.35) * 7.0 + warpA * 0.12) - 0.5) * 22)
          : clamp(1 - Math.abs(fract(field * 5.0 + warpA * 0.15) - 0.5) * 18);
      color = mixColor(color, palette.mark, contour * (input.aspect === 'thumb' ? 0.025 : 0.040) + motif.line);

      const grain = hash2(px, py, detailSeed) - 0.5;
      const grainAmount = input.aspect === 'banner' ? 7 : 5;
      const index = (py * width + px) * 4;
      data[index] = clampByte(color.r + grain * grainAmount);
      data[index + 1] = clampByte(color.g + grain * grainAmount);
      data[index + 2] = clampByte(color.b + grain * grainAmount);
      data[index + 3] = 255;
    }
  }

  const canvas = document.createElement('canvas');
  const ctx = canvas.getContext('2d');
  if (!ctx) throw new Error('Could not create art canvas context');
  canvas.width = width;
  canvas.height = height;
  ctx.putImageData(image, 0, 0);
  drawVersionIdentity(ctx, input, palette, width, height);
  return { source: canvas, width, height };
}

export function renderInstanceArt(input: RenderInput): RenderedArt {
  const key = cacheKey(input);
  const cached = cache.get(key);
  if (cached) {
    cache.delete(key);
    cache.set(key, cached);
    return cached;
  }

  const rendered = renderPixels(input);
  cache.set(key, rendered);
  const byteSize = cacheByteSize(input);
  cacheSizes.set(key, byteSize);
  cacheBytes += byteSize;
  enforceCacheLimit();
  return rendered;
}
