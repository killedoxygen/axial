import { clamp01, hash3 } from './light';
import type { View } from './sky';
import { quarter } from './strips';

/** Blurs `src` into `dst` along one axis by a running mean. `step` is the distance between neighbours on that axis. */
function blur(
  src: Float32Array,
  dst: Float32Array,
  width: number,
  height: number,
  across: boolean,
  radius: number,
): void {
  const length = across ? width : height;
  const lines = across ? height : width;
  const step = across ? 3 : width * 3;
  const lineStride = across ? width * 3 : 3;
  for (let line = 0; line < lines; line++) {
    for (let c = 0; c < 3; c++) {
      const start = line * lineStride + c;
      let sum = 0;
      for (let i = -radius; i <= radius; i++) sum += src[start + Math.min(length - 1, Math.max(0, i)) * step]!;
      for (let i = 0; i < length; i++) {
        dst[start + i * step] = sum / (radius * 2 + 1);
        sum += src[start + Math.min(length - 1, i + radius + 1) * step]! - src[start + Math.max(0, i - radius) * step]!;
      }
    }
  }
}

/** The light of columns `from` up to `to`, held from column `origin` for `wide` columns: a column of margin each side. */
export interface Lit {
  origin: number;
  wide: number;
  from: number;
  to: number;
  hdr: Float32Array;
}

/**
 * Edges that run up the picture fall exactly between columns, hard but never stepped. Softens them a little where
 * something near ends against something far; span ends were already given their exact coverage.
 */
export function soften({ origin, wide, from, to, hdr }: Lit, depths: Float32Array, W: number, H: number): void {
  const line = new Float32Array(wide * 3);
  const first = Math.max(from, 1) - origin;
  const last = Math.min(to, W - 1) - origin;
  for (let y = 0; y < H; y++) {
    line.set(hdr.subarray(y * wide * 3, (y + 1) * wide * 3));
    for (let x = first; x < last; x++) {
      const i = y * wide + x;
      const d = depths[i]!;
      const gap = Math.max(Math.abs(depths[i - 1]! - d), Math.abs(depths[i + 1]! - d));
      if (gap < d * 0.12 + 0.3) continue;
      for (let c = x * 3; c < x * 3 + 3; c++)
        hdr[y * wide * 3 + c] = line[c]! * 0.6 + (line[c - 3]! + line[c + 3]!) * 0.2;
    }
  }
}

/** A strip's own columns at quarter size: its share of what the bloom blurs. */
export function quartered({ origin, wide, from, to, hdr }: Lit, H: number): Float32Array {
  const across = quarter(to) - (from >> 2);
  const soft = new Float32Array(across * quarter(H) * 3);
  for (let y = 0; y < H; y++) {
    for (let x = from; x < to; x++) {
      const o = (y * wide + x - origin) * 3;
      const b = ((y >> 2) * across + (x >> 2) - (from >> 2)) * 3;
      soft[b] += hdr[o]! / 16;
      soft[b + 1] += hdr[o + 1]! / 16;
      soft[b + 2] += hdr[o + 2]! / 16;
    }
  }
  return soft;
}

/** Bloom: the whole picture at quarter size, blurred, to be laid back over itself at a fifth. */
export function bloom(sharp: Float32Array, qw: number, qh: number): Float32Array {
  const soft = sharp.slice();
  const scratch = new Float32Array(soft.length);
  for (const radius of [1, 2, 5]) {
    blur(soft, scratch, qw, qh, true, radius);
    blur(scratch, soft, qw, qh, false, radius);
  }
  return soft;
}

// The tone curve ends in display values. They are tabled by the square root of the light, which keeps the darkest
// tones as finely stepped as the brightest; a dusk would otherwise fall into bands.
const display = new Float32Array(2049);
for (let i = 0; i <= 2048; i++) display[i] = 255 * (i / 2048) ** (2 / 2.2);
/** Light as it is up to four fifths of full; past that it rolls off and never quite gets there. */
const rolled = (value: number): number => (value <= 0.8 ? value : 0.8 + 0.2 * (1 - Math.exp(-(value - 0.8) / 0.2)));
const shown = (value: number): number => display[(Math.sqrt(clamp01(value)) * 2048) | 0]!;

/**
 * A strip's light as display pixels: bloom laid over it, then a tone curve (let the brightest colours drift toward
 * white, roll off smoothly), display values and dither.
 */
export function tone(
  { origin, wide, from, to, hdr }: Lit,
  view: View,
  soft: Float32Array,
): Uint8ClampedArray<ArrayBuffer> {
  const H = view.height;
  const qw = quarter(view.width);
  const qh = quarter(H);
  // The inverse of the drift toward white, which gives the colour back after the roll-off.
  const [p, q, s] = [1.0428, -0.033, -0.0098];
  const mid = 1.0659;
  const across = to - from;
  const out = new Uint8ClampedArray(across * H * 4);
  const mixed = [0, 0, 0];
  for (let y = 0; y < H; y++) {
    const gy = Math.max(0, (y - 1.5) / 4);
    const y0 = Math.min(qh - 1, gy | 0);
    const y1 = Math.min(qh - 1, y0 + 1);
    const fy = gy - y0;
    for (let x = from; x < to; x++) {
      const gx = Math.max(0, (x - 1.5) / 4);
      const x0 = Math.min(qw - 1, gx | 0);
      const x1 = Math.min(qw - 1, x0 + 1);
      const fx = gx - x0;
      const a = y0 * qw + x0;
      const b = y0 * qw + x1;
      const c = y1 * qw + x0;
      const d = y1 * qw + x1;
      const o = (y * wide + x - origin) * 3;
      for (let ch = 0; ch < 3; ch++) {
        const upper = soft[a * 3 + ch]! + (soft[b * 3 + ch]! - soft[a * 3 + ch]!) * fx;
        const lower = soft[c * 3 + ch]! + (soft[d * 3 + ch]! - soft[c * 3 + ch]!) * fx;
        mixed[ch] = hdr[o + ch]! * 0.92 + (upper + (lower - upper) * fy) * 0.08;
      }
      const r = mixed[0]!;
      const g = mixed[1]!;
      const bl = mixed[2]!;
      const tr = rolled(r * 0.96 + g * 0.03 + bl * 0.01);
      const tg = rolled(r * 0.03 + g * 0.94 + bl * 0.03);
      const tb = rolled(r * 0.01 + g * 0.03 + bl * 0.96);
      const noise = (hash3(x, y, 5) - 0.5) * 1.6;
      const pxl = (y * across + x - from) * 4;
      out[pxl] = shown(tr * p + tg * q + tb * s) + noise;
      out[pxl + 1] = shown(tr * q + tg * mid + tb * q) + noise;
      out[pxl + 2] = shown(tr * s + tg * q + tb * p) + noise;
      out[pxl + 3] = 255;
    }
  }
  return out;
}
