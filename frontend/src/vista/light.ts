import { TEXELS } from './textures';
import { FACES, FOLIAGE, GRID, LAVA, PLANT, RUNS, type World } from './world';

export type Rgb = readonly [number, number, number];

export const clamp01 = (value: number): number => (value < 0 ? 0 : value > 1 ? 1 : value);

export function hash3(x: number, y: number, z: number): number {
  let h = Math.imul(x, 0x27d4eb2d) ^ Math.imul(y, 0x165667b1) ^ Math.imul(z, 0x9e3779b9);
  h = Math.imul(h ^ (h >>> 15), 0x85ebca6b);
  h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35);
  return ((h ^ (h >>> 16)) >>> 0) / 4294967296;
}

/** A shader pack's colour setting: display red, green and blue with a strength. */
const setting = (r: number, g: number, b: number, strength: number): Rgb => [
  (r / 255) * strength,
  (g / 255) * strength,
  (b / 255) * strength,
];
/** Shader packs hold their colours as square roots; the light itself is the square. */
const squared = (color: Rgb): Rgb => [color[0] ** 2, color[1] ** 2, color[2] ** 2];
const blend = (a: Rgb, b: Rgb, t: number): Rgb => [
  a[0] + (b[0] - a[0]) * t,
  a[1] + (b[1] - a[1]) * t,
  a[2] + (b[2] - a[2]) * t,
];

/**
 * The light of one hour, by a shader pack's own settings and blends: warm and low at either end of the day,
 * blue-white at noon, blue under the moon.
 */
export class Light {
  /** How much of the day is left: one with the sun well up, zero once it is well down. */
  readonly daylight: number;
  /** How near the hour is to noon. */
  readonly noon: number;
  /** Whether the sun casts the shadows; otherwise the moon, opposite it, does. */
  readonly sunUp: boolean;
  /** Unit vector toward the sun. */
  readonly sun: Rgb;
  /** Unit vector toward whichever of the two casts the shadows, and its direction over the ground. */
  readonly x: number;
  readonly y: number;
  readonly z: number;
  readonly towardX: number;
  readonly towardZ: number;
  /** The light's run across the ground, and how steeply it climbs over that. */
  readonly flat: number;
  readonly rise: number;
  /** The sun's colour as the pack holds it, and to the power the sky near it takes. */
  readonly sunRoot: Rgb;
  readonly sunSky: Rgb;
  /** What the sun or moon gives a surface it reaches, and what the sky gives one it does not. */
  readonly color: Rgb;
  readonly ambient: Rgb;
  readonly night: Rgb;
  readonly nightHue: Rgb;
  readonly lamp = squared(setting(255, 212, 160, 0.85));
  readonly skyBlue = squared(setting(96, 160, 255, 1));
  readonly water = squared(setting(64, 160, 255, 0.35));
  readonly skyExposure: number;
  /** How strongly shafts of light show in the air. */
  readonly rays: number;

  constructor(world: World) {
    const [sunX, sunY, sunZ] = (this.sun = world.sun);
    this.daylight = clamp01(sunY * 10 + 0.5);
    this.noon = Math.max(0, Math.sin(world.time * 2 * Math.PI));
    const high = 1 - (1 - this.noon) ** 1.5;
    const moon = setting(104, 168, 255, 0.44);
    this.sunRoot = blend(setting(255, 160, 80, 1.2), setting(196, 220, 255, 1.4), high);
    this.color = squared(blend(moon, this.sunRoot, this.daylight));
    this.ambient = squared(
      blend(
        [moon[0] * 0.6, moon[1] * 0.6, moon[2] * 0.6],
        blend(setting(255, 204, 144, 0.35), setting(120, 172, 255, 0.6), high),
        this.daylight,
      ),
    );
    this.night = squared(moon);
    const length = Math.hypot(...this.night);
    this.nightHue = [this.night[0] / length, this.night[1] / length, this.night[2] / length];
    this.sunUp = world.time < 0.5325 || world.time > 0.9675;
    const side = this.sunUp ? 1 : -1;
    this.x = sunX * side;
    this.y = sunY * side;
    this.z = sunZ * side;
    const flat = (this.flat = Math.hypot(this.x, this.z) || 1);
    this.towardX = this.x / flat;
    this.towardZ = this.z / flat;
    this.rise = Math.max(0, this.y) / flat;
    this.skyExposure = 2 ** (this.noon * 0.75 - 0.75);
    const power = 4 - this.daylight;
    this.sunSky = [this.sunRoot[0] ** power, this.sunRoot[1] ** power, this.sunRoot[2] ** power];
    this.rays = this.sunUp ? 0.9 - 0.75 * this.noon : 0.3;
  }
}

/**
 * What stands in each column of the world: the highest solid block, the highest foliage, and the highest solid
 * block among its eight neighbours, which is all the corner shading needs to know to skip open ground.
 */
export class Tops {
  readonly solid = new Uint8Array(GRID * GRID);
  readonly leaf = new Uint8Array(GRID * GRID);
  readonly rim = new Uint8Array(GRID * GRID);

  constructor({ ground, runs, runCount }: World) {
    const n = GRID;
    const { solid, leaf, rim } = this;
    for (let i = 0; i < n * n; i++) {
      solid[i] = ground[i]!;
      for (let k = 0; k < runCount[i]!; k++) {
        const ro = (i * RUNS + k) * 3;
        const mat = runs[ro + 2]!;
        if (mat >= PLANT) continue;
        if (mat >= FOLIAGE && mat < LAVA) leaf[i] = runs[ro + 1]!;
        else solid[i] = Math.max(solid[i]!, runs[ro + 1]!);
      }
    }
    const around = [-1, 1, -n, n, -n - 1, -n + 1, n - 1, n + 1];
    for (let z = 1; z < n - 1; z++) {
      for (let x = 1; x < n - 1; x++) {
        const i = z * n + x;
        for (const step of around) if (solid[i + step]! > rim[i]!) rim[i] = solid[i + step]!;
      }
    }
  }
}

/**
 * The shadow map: the world in front of the camera as the light sees it. Along a ray of light two things stay
 * fixed, so they index the map; each texel keeps the height of the highest thing on its ray, and a point is lit
 * where nothing on its ray stands higher. Foliage casts the shape of its texture, gaps and all.
 */
export class Shadows {
  private size = 0;
  private rows = 0;
  private scale = 0;
  private slope = 0;
  private u0 = 0;
  private v0 = 0;
  private map = new Float32Array(0);
  private readonly towardX: number;
  private readonly towardZ: number;

  constructor(world: World, light: Light, tops: Tops) {
    const n = GRID;
    const { ground, runs, runCount, camera } = world;
    const atlas = world.atlas.colors;
    const { towardX, towardZ } = light;
    this.towardX = towardX;
    this.towardZ = towardZ;
    if (light.y <= 0.03) return;
    const slope = (this.slope = Math.min(14, light.flat / light.y));
    const x0 = Math.max(1, Math.floor(camera.x) - 66);
    const x1 = Math.min(n - 1, x0 + 132);
    const z0 = Math.max(1, Math.floor(camera.z) - 10);
    const z1 = Math.min(n - 1, z0 + 140);
    let low = 255;
    let tall = 0;
    for (let z = z0; z < z1; z++) {
      for (let x = x0; x < x1; x++) {
        const i = z * n + x;
        if (ground[i]! < low) low = ground[i]!;
        const most = Math.max(tops.solid[i]!, tops.leaf[i]!);
        if (most > tall) tall = most;
      }
    }
    let uMin = Infinity;
    let uMax = -Infinity;
    let vMin = Infinity;
    let vMax = -Infinity;
    for (const x of [x0, x1]) {
      for (const z of [z0, z1]) {
        const u = x * towardX + z * towardZ;
        const v = z * towardX - x * towardZ;
        uMin = Math.min(uMin, u - tall * slope);
        uMax = Math.max(uMax, u - low * slope);
        vMin = Math.min(vMin, v);
        vMax = Math.max(vMax, v);
      }
    }
    this.u0 = uMin;
    this.v0 = vMin;
    const scale = (this.scale = Math.min(4, Math.sqrt(1_000_000 / ((uMax - uMin) * (vMax - vMin)))));
    const size = (this.size = Math.ceil((uMax - uMin) * scale) + 2);
    this.rows = Math.ceil((vMax - vMin) * scale) + 2;
    const map = (this.map = new Float32Array(size * this.rows).fill(-1));
    // Each block is sampled finely enough to leave no texel uncovered, and each sample's whole height is cast at
    // once: along its ray it is a run of texels, highest first.
    const fine = Math.ceil(scale * 1.5);
    const cast = (px: number, pz: number, from: number, to: number): void => {
      const along = px * towardX + pz * towardZ - uMin;
      const row = Math.floor((pz * towardX - px * towardZ - vMin) * scale) * size;
      const first = Math.floor((along - to * slope) * scale);
      const last = Math.floor((along - from * slope) * scale);
      for (let u = first; u <= last; u++) {
        const y = u === first ? to : (along - u / scale) / slope;
        if (y > map[row + u]!) map[row + u] = y;
      }
    };
    for (let z = z0; z < z1; z++) {
      for (let x = x0; x < x1; x++) {
        const i = z * n + x;
        const g = ground[i]!;
        // Below the lowest neighbour a column's side is buried, and casts nothing that is not already cast.
        const buried = Math.max(g - 12, Math.min(g, ground[i - 1]!, ground[i + 1]!, ground[i - n]!, ground[i + n]!));
        const count = runCount[i]!;
        for (let s = 0; s < fine * fine; s++) {
          const fx = ((s % fine) + 0.5) / fine;
          const fz = (Math.floor(s / fine) + 0.5) / fine;
          cast(x + fx, z + fz, buried, g);
          for (let k = 0; k < count; k++) {
            const ro = (i * RUNS + k) * 3;
            const mat = runs[ro + 2]!;
            if (mat >= PLANT) continue;
            if (mat >= FOLIAGE && mat < LAVA) {
              const texel = (FACES[mat * 3]! * TEXELS + Math.floor(fz * 16) * 16 + Math.floor(fx * 16)) * 4;
              if (atlas[texel + 3]! < 0.5) continue;
            }
            cast(x + fx, z + fz, runs[ro]!, runs[ro + 1]!);
          }
        }
      }
    }
  }

  /** How much of the light reaches a point: the four texels round it, each lit or not, blended. */
  lit(x: number, y: number, z: number): number {
    const { size, map } = this;
    if (size === 0) return 0;
    const u = (x * this.towardX + z * this.towardZ - y * this.slope - this.u0) * this.scale - 0.5;
    const v = (z * this.towardX - x * this.towardZ - this.v0) * this.scale - 0.5;
    const iu = Math.floor(u);
    const iv = Math.floor(v);
    // Beyond the map the world is too far for its shadows to read.
    if (iu < 0 || iv < 0 || iu >= size - 1 || iv >= this.rows - 1) return 1;
    const fu = u - iu;
    const fv = v - iv;
    const at = iv * size + iu;
    const level = y + 0.06;
    return (
      (level >= map[at]! ? (1 - fu) * (1 - fv) : 0) +
      (level >= map[at + 1]! ? fu * (1 - fv) : 0) +
      (level >= map[at + size]! ? (1 - fu) * fv : 0) +
      (level >= map[at + size + 1]! ? fu * fv : 0)
    );
  }
}
