import { ANDESITE_TEX, CLAY_PLAIN, COAL_TEX, GRANITE_TEX, MIP_OFFSET, STONE_TEX, TEXELS } from './textures';
import {
  BANDS,
  BELOW,
  FACES,
  FOLIAGE,
  GRID,
  LAVA,
  PLANT,
  RUNS,
  TERRACOTTA,
  VINE,
  WATER,
  valueNoise,
  type World,
} from './world';

/**
 * Draws a world to pixels on the CPU, cheaply enough for the slowest machine, lit the way the game's shader packs
 * light it.
 *
 * The camera never rolls or pitches (the horizon is placed by shifting the image instead), so every block edge that
 * is vertical in the world is vertical on screen. A screen column therefore crosses the grid along one 2D line: walk
 * that line once, front to back, and each column of blocks it meets projects to a few vertical spans (the face
 * toward the camera, a plant standing in the cell, then the top or underside). Cost follows the number of pixels,
 * not rays times steps. Water mirrors the same walk into the pixels the first pass marked as water.
 *
 * Light follows the shader packs' own model. A surface takes the sky's colour in shade and the sun's or moon's
 * where their light reaches it, both scaled by the game's sky-light level; glowing blocks add the game's block
 * light, bent so that it is hot at the source and trails off warm; then the game's own shading of each face and of
 * each inner corner is laid over the lot. Shadows come from a map of the world seen from the light.
 */
export interface Renderer {
  readonly width: number;
  readonly height: number;
  /** Draws screen columns `from` up to `to`. Columns are independent, so the caller may spread them over frames. */
  columns(from: number, to: number): void;
  /** Light shafts, bloom, edge smoothing, tone and dither. Call once every column is drawn. */
  finish(): Uint8ClampedArray<ArrayBuffer>;
  /** The same finishing work in short steps, for a caller that must not hold the thread; the last step returns the picture. */
  finishing(): Generator<void, Uint8ClampedArray<ArrayBuffer>>;
}

const NEAR = 0.2;
const FAR = 260;
/** As wide as the game's own default view. */
const FOV = 1.72;
const TOP = 0;
const SIDE_X = 1;
const SIDE_Z = 2;
const UNDER = 3;
/** The game's own shading of a block's faces, squared as shader packs apply it: top, the two pairs of sides, underside. */
const DIFFUSE = [1, 0.49, 0.7225, 0.0625];

function hash3(x: number, y: number, z: number): number {
  let h = Math.imul(x, 0x27d4eb2d) ^ Math.imul(y, 0x165667b1) ^ Math.imul(z, 0x9e3779b9);
  h = Math.imul(h ^ (h >>> 15), 0x85ebca6b);
  h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35);
  return ((h ^ (h >>> 16)) >>> 0) / 4294967296;
}

/** A shader pack's colour setting: display red, green and blue with a strength, held squared. */
const tint = (r: number, g: number, b: number, strength: number): [number, number, number] => [
  ((r / 255) * strength) ** 2,
  ((g / 255) * strength) ** 2,
  ((b / 255) * strength) ** 2,
];
const blend = (a: readonly number[], b: readonly number[], t: number): [number, number, number] => [
  a[0]! + (b[0]! - a[0]!) * t,
  a[1]! + (b[1]! - a[1]!) * t,
  a[2]! + (b[2]! - a[2]!) * t,
];
const clamp01 = (value: number): number => (value < 0 ? 0 : value > 1 ? 1 : value);

export function createRenderer(world: World, width: number, height: number, horizon = 0.5): Renderer {
  const n = GRID;
  const { ground, groundMat, runs, runCount, decal, blockLight, skyLight, camera } = world;
  const atlas = world.atlas.colors;
  const glow = world.atlas.glow;
  const W = width;
  const H = height;
  const focal = W / 2 / Math.tan(FOV / 2);
  const hz = H * horizon;
  const cx = camera.x;
  const cy = camera.y;
  const cz = camera.z;
  const fwdX = Math.sin(camera.yaw);
  const fwdZ = Math.cos(camera.yaw);
  const hasWater = world.waterLevel > 0;
  const surface = world.waterLevel - 0.14;

  // The hour decides everything about the light. These are a shader pack's own settings and blends: warm and low
  // at either end of the day, blue-white at noon, a dim blue under the moon.
  const [sunX, sunY, sunZ] = world.sun;
  const daylight = clamp01(sunY * 10 + 0.5);
  const noon = Math.max(0, Math.sin(world.time * 2 * Math.PI));
  const high = 1 - (1 - noon) ** 1.5;
  const lowSun = [(255 / 255) * 1.2, (160 / 255) * 1.2, (80 / 255) * 1.2];
  const highSun = [(196 / 255) * 1.4, (220 / 255) * 1.4, 1.4];
  const moonlight = [(96 / 255) * 0.3, (192 / 255) * 0.3, 0.3];
  const sunRoot = blend(lowSun, highSun, high);
  const lightColor = blend(moonlight, sunRoot, daylight).map((c) => c * c);
  const ambientColor = blend(
    moonlight.map((c) => c * 0.6),
    blend([0.35, (204 / 255) * 0.35, (144 / 255) * 0.35], [(120 / 255) * 0.6, (172 / 255) * 0.6, 0.6], high),
    daylight,
  ).map((c) => c * c);
  const lampColor = tint(255, 212, 160, 0.85);
  const skyBlue = tint(96, 160, 255, 1);
  const nightBlue = moonlight.map((c) => c * c);
  const nightHue = nightBlue.map((c) => c / Math.hypot(nightBlue[0]!, nightBlue[1]!, nightBlue[2]!));
  const waterColor = tint(64, 160, 255, 0.35);
  const MIN_LIGHT = 0.063;
  // The light that casts shadows: the sun while it is up, then the moon opposite it.
  const sunUp = world.time < 0.5325 || world.time > 0.9675;
  const lightX = sunUp ? sunX : -sunX;
  const lightY = sunUp ? sunY : -sunY;
  const lightZ = sunUp ? sunZ : -sunZ;
  const lightAhead = lightX * fwdX + lightZ * fwdZ > 0;
  const lightAzimuth = Math.atan2(lightX, lightZ) - camera.yaw;
  const lightFlat = Math.hypot(lightX, lightZ) || 1;
  const lightRise = Math.max(0, lightY) / lightFlat;
  const skyExposure = 2 ** (noon * 0.75 - 0.75);
  const sunSky = sunRoot.map((c) => c ** (4 - daylight));
  const rays = sunUp ? 0.9 - 0.75 * noon : 0.3;

  const hdr = new Float32Array(W * H * 3);
  // Depth per pixel, so edges between near and far can be smoothed without softening the textures.
  const depths = new Float32Array(W * H);
  const mirror = hasWater ? new Float32Array(H * 3) : null;
  const waterDepth = new Float32Array(H);
  const waterCell = new Int32Array(H);
  // Open sky near the light at quarter size: the source of the light shafts.
  const qw = Math.ceil(W / 4);
  const qh = Math.ceil(H / 4);
  const openSky = new Float32Array(qw * qh);

  // What stands in each column: the highest solid block, the highest foliage, and the highest solid block among
  // its eight neighbours, which is all the corner shading needs to know to skip open ground.
  const solidTop = new Uint8Array(n * n);
  const leafTop = new Uint8Array(n * n);
  const rim = new Uint8Array(n * n);
  for (let i = 0; i < n * n; i++) {
    let solid = ground[i]!;
    let leaf = 0;
    for (let k = 0; k < runCount[i]!; k++) {
      const ro = (i * RUNS + k) * 3;
      const mat = runs[ro + 2]!;
      if (mat >= PLANT) continue;
      if (mat >= FOLIAGE && mat < LAVA) leaf = runs[ro + 1]!;
      else solid = Math.max(solid, runs[ro + 1]!);
    }
    solidTop[i] = solid;
    leafTop[i] = leaf;
  }
  for (let z = 1; z < n - 1; z++) {
    for (let x = 1; x < n - 1; x++) {
      const i = z * n + x;
      rim[i] = Math.max(
        solidTop[i - 1]!,
        solidTop[i + 1]!,
        solidTop[i - n]!,
        solidTop[i + n]!,
        solidTop[i - n - 1]!,
        solidTop[i - n + 1]!,
        solidTop[i + n - 1]!,
        solidTop[i + n + 1]!,
      );
    }
  }

  // The shadow map: the world in front of the camera as the light sees it. Along a ray of light two things stay
  // fixed, so they index the map; each texel keeps the height of the highest thing on its ray, and a point is lit
  // where nothing on its ray stands higher. Foliage casts the shape of its texture, gaps and all.
  let shadowSize = 0;
  let shadowRows = 0;
  let shadowScale = 0;
  let shadowSlope = 0;
  let shadowU = 0;
  let shadowV = 0;
  let shadows = new Float32Array(0);
  const towardX = lightX / lightFlat;
  const towardZ = lightZ / lightFlat;
  if (lightY > 0.03) {
    shadowSlope = Math.min(14, lightFlat / lightY);
    const x0 = Math.max(1, Math.floor(cx) - 66);
    const x1 = Math.min(n - 1, x0 + 132);
    const z0 = Math.max(1, Math.floor(cz) - 10);
    const z1 = Math.min(n - 1, z0 + 140);
    let low = 255;
    let tall = 0;
    for (let z = z0; z < z1; z++) {
      for (let x = x0; x < x1; x++) {
        const i = z * n + x;
        if (ground[i]! < low) low = ground[i]!;
        const most = Math.max(solidTop[i]!, leafTop[i]!);
        if (most > tall) tall = most;
      }
    }
    let uMin = Infinity;
    let uMax = -Infinity;
    let vMin = Infinity;
    let vMax = -Infinity;
    for (const [x, z] of [
      [x0, z0],
      [x1, z0],
      [x0, z1],
      [x1, z1],
    ] as const) {
      const u = x * towardX + z * towardZ;
      const v = z * towardX - x * towardZ;
      uMin = Math.min(uMin, u - tall * shadowSlope);
      uMax = Math.max(uMax, u - low * shadowSlope);
      vMin = Math.min(vMin, v);
      vMax = Math.max(vMax, v);
    }
    shadowU = uMin;
    shadowV = vMin;
    shadowScale = Math.min(4, Math.sqrt(1_000_000 / ((uMax - uMin) * (vMax - vMin))));
    shadowSize = Math.ceil((uMax - uMin) * shadowScale) + 2;
    shadowRows = Math.ceil((vMax - vMin) * shadowScale) + 2;
    shadows = new Float32Array(shadowSize * shadowRows).fill(-1);
    // Each block is sampled finely enough to leave no texel uncovered, and each sample's whole height is cast at
    // once: along its ray it is a run of texels, highest first.
    const fine = Math.ceil(shadowScale * 1.5);
    const cast = (px: number, pz: number, from: number, to: number): void => {
      const along = px * towardX + pz * towardZ - shadowU;
      const row = Math.floor((pz * towardX - px * towardZ - shadowV) * shadowScale) * shadowSize;
      const first = Math.floor((along - to * shadowSlope) * shadowScale);
      const last = Math.floor((along - from * shadowSlope) * shadowScale);
      for (let u = first; u <= last; u++) {
        const y = u === first ? to : (along - u / shadowScale) / shadowSlope;
        if (y > shadows[row + u]!) shadows[row + u] = y;
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
  const lit = (x: number, y: number, z: number): number => {
    if (shadowSize === 0) return 0;
    const u = (x * towardX + z * towardZ - y * shadowSlope - shadowU) * shadowScale - 0.5;
    const v = (z * towardX - x * towardZ - shadowV) * shadowScale - 0.5;
    const iu = Math.floor(u);
    const iv = Math.floor(v);
    // Beyond the map the world is too far for its shadows to read.
    if (iu < 0 || iv < 0 || iu >= shadowSize - 1 || iv >= shadowRows - 1) return 1;
    const fu = u - iu;
    const fv = v - iv;
    const at = iv * shadowSize + iu;
    const level = y + 0.06;
    return (
      (level >= shadows[at]! ? (1 - fu) * (1 - fv) : 0) +
      (level >= shadows[at + 1]! ? fu * (1 - fv) : 0) +
      (level >= shadows[at + shadowSize]! ? (1 - fu) * fv : 0) +
      (level >= shadows[at + shadowSize + 1]! ? fu * fv : 0)
    );
  };

  // Rock is not one stone: veins of andesite and granite run through it.
  const veins = new Uint8Array(n * n);
  for (let z = 0; z < n; z++) {
    for (let x = 0; x < n; x++) {
      const vein = valueNoise(x / 7, z / 7, world.seed + 201);
      veins[z * n + x] = vein > 0.74 ? ANDESITE_TEX : vein < 0.2 ? GRANITE_TEX : STONE_TEX;
    }
  }

  // The water's swell: trains of waves of several lengths crossing at angles round the wind.
  // Each is (x step, z step, height, length), longest first.
  const WAVES = 6;
  const waves = new Float32Array(WAVES * 4);
  {
    const wind = hash3(world.seed, 3, 5) * 6.283;
    const lengths = [7.5, 4.6, 3.1, 2.1, 1.3, 0.8];
    const slopes = [0.035, 0.045, 0.05, 0.045, 0.03, 0.018];
    for (let i = 0; i < WAVES; i++) {
      const turn = wind + (hash3(world.seed, i, 9) - 0.5) * 2.6;
      const k = 6.283 / lengths[i]!;
      waves.set([Math.sin(turn) * k, Math.cos(turn) * k, slopes[i]! / k, lengths[i]!], i * 4);
    }
  }
  let slopeX = 0;
  let slopeZ = 0;
  let chop = 0;
  /**
   * The slope of the water at a point, into `slopeX` and `slopeZ`. `grain` is how much water one pixel spans there:
   * waves too short to show at that size are left out and counted in `chop`, which spreads the sun's glitter instead.
   */
  const swell = (x: number, z: number, grain: number): void => {
    // The trains bend a little, or they would lie as straight as corduroy.
    const qx = x + Math.sin(z * 0.23 + 1.7) * 0.9;
    const qz = z + Math.sin(x * 0.19 + 0.4) * 0.9;
    // The water lies calmer in some stretches than in others.
    const gust = 0.55 + 0.9 * valueNoise(x / 11, z / 11, world.seed + 91);
    slopeX = 0;
    slopeZ = 0;
    chop = 0;
    for (let o = 0; o < WAVES * 4; o += 4) {
      const kx = waves[o]!;
      const kz = waves[o + 1]!;
      const shown = waves[o + 3]! / (grain * 5);
      const steep = waves[o + 2]! * gust * Math.cos(kx * qx + kz * qz + o * 1.7);
      if (shown < 1) {
        const rest = waves[o + 2]! * waves[o + 2]! * (kx * kx + kz * kz);
        chop += rest * (1 - shown * shown);
        if (shown < 0.1) continue;
        slopeX += steep * kx * shown;
        slopeZ += steep * kz * shown;
      } else {
        slopeX += steep * kx;
        slopeZ += steep * kz;
      }
    }
  };

  // How much of each pixel of the column is still uncovered: edges take their exact share, so they come out smooth.
  const left = new Float32Array(H);
  const waterShare = new Float32Array(H);
  const bedTap = new Float32Array(3);
  const mirrored = new Uint8Array(H);
  const pathCell = new Int32Array(4 * n);
  const pathEnter = new Float32Array(4 * n);
  const pathExit = new Float32Array(4 * n);
  const pathFace = new Uint8Array(4 * n);

  // Per-column state the shading closures read.
  let col = 0;
  let dirX = 0;
  let dirZ = 0;
  let span = 1;
  let glare = 0;
  let reach = FAR;
  let fogR = 0;
  let fogG = 0;
  let fogB = 0;
  let angle = 0;
  let scatter = 0;
  let lo = 0;
  let hi = 0;

  const skyTap = new Float32Array(3);
  const cloudAt = (x: number, z: number): number =>
    valueNoise(x, z * 1.25, world.seed + 71) * 0.56 +
    valueNoise(x * 2.3 + 5, z * 2.9, world.seed + 73) * 0.29 +
    valueNoise(x * 5.3, z * 6.1 + 3, world.seed + 75) * 0.15;

  /**
   * The sky's own colour in a direction, without sun, moon or cloud: `up` is how steeply the direction climbs and
   * `toSun` how nearly it points at the sun, both as cosines. Blue thinning toward the horizon by day, the sun's
   * colour spreading along the horizon when it is low, a faint blue at night.
   */
  const skyColor = (out: Float32Array, up: number, toSun: number): void => {
    const lift = up > 0 ? up : 0;
    const gradient = Math.exp(-(1 - (1 - lift) ** (1.5 - 0.5 * toSun)) / 0.35);
    const sunward = ((toSun * 0.5 + 0.5) * (1 - lift)) ** (2 - daylight) * (1 - noon * 0.6) ** 3;
    const mix = 1 - (1 - sunward) * (1 - (1 - lift) ** 2.5 * 0.125);
    const night = Math.exp(-lift / 0.65) * 0.0884;
    for (let c = 0; c < 3; c++) {
      const blue = skyBlue[c]! * gradient;
      const day = (blue / Math.sqrt(blue * blue + 1)) * skyExposure * daylight;
      const both = Math.sqrt(day * (1 - mix)) * (1 - mix) + Math.sqrt(sunSky[c]! * gradient) * mix;
      const dark = nightBlue[c]! * night;
      out[c] = dark + (both * both - dark) * daylight * daylight;
    }
  };

  const skyInto = (out: Float32Array, o: number, row: number, record: boolean): void => {
    const v = (hz - row - 0.5) / focal;
    const length = Math.sqrt(span + v * v);
    const up = Math.max(0, v) / length;
    const toSun = (dirX * sunX + Math.max(0, v) * sunY + dirZ * sunZ) / length;
    skyColor(skyTap, up, toSun);
    let r = skyTap[0]!;
    let g = skyTap[1]!;
    let b = skyTap[2]!;
    const da = angle - lightAzimuth;
    const dv = Math.max(0, v) - lightRise;
    const halo = lightAhead ? Math.exp(-(da * da * 1.4 + dv * dv * 6)) : 0;
    if (daylight < 1 && v > 0.03) {
      const star = hash3(col >> 1, Math.floor(row) >> 1, world.seed);
      if (star > 0.996) {
        const k = (0.2 + hash3(col, Math.floor(row), 7) * 0.5) * Math.min(1, v * 4) * (1 - daylight);
        r += k;
        g += k;
        b += k * 1.1;
      }
    }
    // The sun is a soft-rimmed disc far brighter than white, so that it blooms; the moon a smaller, paler one.
    if (toSun > 0.99) {
      const disc = clamp01((toSun - 0.993) / 0.007) ** 3 * 3.5 + toSun ** 192 * 0.15;
      const fade = clamp01(up * 12);
      r += sunRoot[0]! * sunRoot[0]! * disc * fade * daylight;
      g += sunRoot[1]! * sunRoot[1]! * disc * fade * daylight;
      b += sunRoot[2]! * sunRoot[2]! * disc * fade * daylight;
    } else if (toSun < -0.995) {
      const disc = (clamp01((-toSun - 0.997) / 0.003) ** 3 * 1.6 + toSun ** 256 * 0.05) * (1 - daylight);
      r += disc * 0.75;
      g += disc * 0.85;
      b += disc;
    }
    // Clouds on a plane overhead: lit by the light on the side toward it and at their rims, by the sky in their
    // thick middles.
    if (v > 0.015) {
      const d = 70 / v;
      if (d < 1800) {
        const wx = (cx + dirX * d) / 130;
        const wz = (cz + dirZ * d) / 130;
        const puff = cloudAt(wx, wz);
        if (puff > 0.52) {
          const cover =
            Math.min(1, (puff - 0.52) * 7) * Math.min(1, v * 10) * Math.max(0, 1 - d / 1800) * (0.35 + 0.65 * daylight);
          const thick = cloudAt(wx + towardX * 0.1, wz + towardZ * 0.1);
          const toLight = sunUp ? toSun : -toSun;
          const glare = (toLight * 0.5 + 0.5) ** 6;
          const bright = clamp01(1.15 - (thick - 0.47) * 3.4) * (1 - Math.min(1, (puff - 0.52) * 4.5) * 0.5);
          const shown = bright + (1 - bright) * (1 - cover * cover) * glare * 0.5;
          const dim = 0.5 - 0.25 * (1 - daylight);
          for (let c = 0; c < 3; c++) {
            const base = ambientColor[c]! * (0.3 * daylight + 0.5);
            const cloud = (base + (lightColor[c]! * (0.85 + 1.15 * glare) - base) * shown) * dim;
            if (c === 0) r += (cloud - r) * cover * cover;
            else if (c === 1) g += (cloud - g) * cover * cover;
            else b += (cloud - b) * cover * cover;
          }
        }
      }
    }
    if (record && row >= 0 && row < H) openSky[(row >> 2) * qw + (col >> 2)] += halo / 16;
    out[o] = r;
    out[o + 1] = g;
    out[o + 2] = b;
  };

  // How much of a far thing the air hides: a thin haze that thickens at night, tabled by distance.
  const hazes = new Float32Array(FAR + 2);
  for (let depth = 0; depth < hazes.length; depth++) {
    let fog = (depth / 1024) * (4 - 3 * daylight);
    if (fog > 0.5) fog = (fog - 0.5) * 0.25 + 0.5;
    hazes[depth] = 1 - Math.exp(-2 * fog ** (0.35 * daylight + 1.25));
  }
  /** The haze at a distance, then the edge of the world, where everything gives way to the sky. */
  const haze = (depth: number): number => {
    const fog = hazes[depth > FAR ? FAR : depth | 0]!;
    const edge = 1 - (reach - depth) / (0.2 * reach);
    return edge > 0 ? fog + (1 - fog) * (edge > 1 ? 1 : edge) : fog;
  };

  /**
   * Textures and lights one surface point, writing it at `o` in `out`. Returns false where the texture is a gap,
   * so the caller leaves the pixel for whatever lies behind.
   * `across` is the position across a plant's cut-out, or -1 on a block face. `texel` is how many pixels one texel
   * covers. `lip` is the surface height when the point is on the side of a ground column, else -1. `foot` is the
   * height a plant is rooted at, and on a top -1 where neighbours cast no corner shade. `inner` dims foliage seen
   * through other foliage.
   */
  const shade = (
    out: Float32Array,
    o: number,
    mat: number,
    face: number,
    wx: number,
    wy: number,
    wz: number,
    depth: number,
    texel: number,
    lip: number,
    foot: number,
    inner: number,
    across: number,
    share: number,
  ): boolean => {
    const bx = Math.floor(wx);
    const bz = Math.floor(wz);
    const flat = face === TOP || face === UNDER;
    const cutout = mat >= FOLIAGE && mat !== LAVA && mat !== WATER;
    const plant = across >= 0;
    const inside = bx >= 0 && bz >= 0 && bx < n && bz < n;
    let tex = FACES[mat * 3 + (plant ? (wy - foot >= 1 ? 0 : 1) : face === TOP ? 0 : face === UNDER ? 2 : 1)]!;
    // Below the surface block a column is soil, then rock.
    if (lip - wy > 1) tex = BELOW[mat * 2 + (lip - wy > 4 ? 1 : 0)]!;
    const by = Math.ceil(wy) - 1;
    if (mat === TERRACOTTA || tex === CLAY_PLAIN) tex = BANDS[by % BANDS.length]!;
    else if (tex === STONE_TEX && inside) {
      const shift = by >> 2;
      tex = hash3(bx, by, bz) > 0.965 ? COAL_TEX : veins[((bz + shift * 7) % n) * n + ((bx + shift * 5) % n)]!;
    }
    // The step from one size of texture to the next is scattered, so it never shows as a line across the ground.
    scatter = (Math.imul(scatter, 1664525) + 1013904223) | 0;
    texel *= 0.8 + (scatter >>> 24) / 640;
    const level = texel >= 0.7 ? 0 : texel >= 0.35 ? 1 : texel >= 0.18 ? 2 : texel >= 0.09 ? 3 : 4;
    const size = 16 >> level;
    let u = plant ? across : face === SIDE_X ? wz - bz : wx - bx;
    let v = flat ? wz - bz : 1 - (wy - Math.floor(wy));
    if (face === TOP && mat <= TERRACOTTA) {
      // The game turns the top of each natural block a random quarter, so no pattern repeats across the ground.
      const turn = (hash3(bx, 7, bz) * 4) | 0;
      if (turn & 1) {
        const swap = u;
        u = v;
        v = 1 - swap;
      }
      if (turn & 2) {
        u = 1 - u;
        v = 1 - v;
      }
    }
    const tu = Math.min(size - 1, (u * size) | 0);
    const tv = Math.min(size - 1, (v * size) | 0);
    const at = tex * TEXELS + MIP_OFFSET[level]! + tv * size + tu;
    const cover = atlas[at * 4 + 3]!;
    if (cutout ? cover < (level < 2 ? 0.5 : plant ? 0.3 : 0.05) : cover === 0) return false;
    let r = atlas[at * 4]!;
    let g = atlas[at * 4 + 1]!;
    let b = atlas[at * 4 + 2]!;
    const shine = glow[at]!;
    // Something that glows is flat-lit by its own light.
    const flatten = shine > 0 ? clamp01(1 - (1 - shine) ** 128) : 0;

    // Light is read in the air in front of the face, as the game reads it.
    const nx = face === SIDE_X && !plant ? (dirX > 0 ? -1 : 1) : 0;
    const nz = face === SIDE_Z && !plant ? (dirZ > 0 ? -1 : 1) : 0;
    const ax = wx + nx * 0.5;
    const az = wz + nz * 0.5;
    const fx = Math.floor(ax);
    const fz = Math.floor(az);
    let sky = 1;
    let lamp = 0;
    let corner = cutout && !plant ? inner : 1;
    if (fx >= 1 && fz >= 1 && fx < n - 2 && fz < n - 2) {
      const fi = fz * n + fx;
      const gx = ax - 0.5;
      const gz = az - 0.5;
      const x0 = Math.floor(gx);
      const z0 = Math.floor(gz);
      const tx = gx - x0;
      const tz = gz - z0;
      const a = z0 * n + x0;
      const w00 = (1 - tx) * (1 - tz);
      const w10 = tx * (1 - tz);
      const w01 = (1 - tx) * tz;
      const w11 = tx * tz;
      // Above everything that stands in the column the sky is open; under it, as thin as the game makes it.
      if (wy < Math.max(leafTop[fi]!, solidTop[fi]!))
        sky = (skyLight[a]! * w00 + skyLight[a + 1]! * w10 + skyLight[a + n]! * w01 + skyLight[a + n + 1]! * w11) / 15;
      const near =
        blockLight[a]! * w00 + blockLight[a + 1]! * w10 + blockLight[a + n]! * w01 + blockLight[a + n + 1]! * w11;
      if (near > 0) lamp = clamp01((near - Math.max(0, wy - ground[fi]! - 0.5)) / 15);
      if (lamp > 0.9333) lamp = 0.9333;
      // The game darkens each inner corner by the solid blocks that meet there, smoothly across the face.
      if (face === TOP) {
        if (foot >= 0 && rim[fi]! > wy + 0.5) {
          const high = wy + 0.5;
          const west = solidTop[fi - 1]! > high ? 0.2 : 1;
          const east = solidTop[fi + 1]! > high ? 0.2 : 1;
          const north = solidTop[fi - n]! > high ? 0.2 : 1;
          const south = solidTop[fi + n]! > high ? 0.2 : 1;
          const meet = (first: number, second: number, diagonal: number): number =>
            (1 + first + second + (first + second < 1 || solidTop[diagonal]! > high ? 0.2 : 1)) / 4;
          const px = wx - bx;
          const pz = wz - bz;
          corner *=
            (meet(west, north, fi - n - 1) * (1 - px) + meet(east, north, fi - n + 1) * px) * (1 - pz) +
            (meet(west, south, fi + n - 1) * (1 - px) + meet(east, south, fi + n + 1) * px) * pz;
        }
      } else if (face !== UNDER && !plant && mat < FOLIAGE) {
        // On a side: the block under the air in front, and the ones to either hand of it.
        const hand = face === SIDE_X ? n : 1;
        const under = solidTop[fi]! > by - 0.5 ? 0.2 : 1;
        const lowLeft = solidTop[fi - hand]!;
        const lowRight = solidTop[fi + hand]!;
        if (under < 1 || lowLeft > by - 0.5 || lowRight > by - 0.5) {
          const leftSide = lowLeft > by + 0.5 ? 0.2 : 1;
          const rightSide = lowRight > by + 0.5 ? 0.2 : 1;
          const meet = (first: number, second: number, diagonal: boolean): number =>
            (1 + first + second + (first + second < 1 || diagonal ? 0.2 : 1)) / 4;
          const across2 = face === SIDE_X ? wz - Math.floor(wz) : wx - Math.floor(wx);
          const rise = wy - by;
          corner *=
            (meet(under, leftSide, lowLeft > by - 0.5) * (1 - across2) +
              meet(under, rightSide, lowRight > by - 0.5) * across2) *
              (1 - rise) +
            (meet(1, leftSide, lowLeft > by + 1.5) * (1 - across2) +
              meet(1, rightSide, lowRight > by + 1.5) * across2) *
              rise;
        }
      }
    }

    // How squarely the face meets the light. Plants stand as if facing straight up; leaves and plants pass light.
    const soft = plant || mat >= PLANT ? 0.5 : cutout ? 1 : 0;
    let facing = plant || face === TOP ? lightY : face === SIDE_X ? nx * lightX : face === SIDE_Z ? nz * lightZ : 0;
    if (facing < 0) facing = 0;
    let through = 0;
    if (soft > 0) {
      through = glare * soft;
      facing += (1 - facing) * Math.sqrt(soft) * 0.7;
      facing += (1 - facing) * through;
    }
    let shadow = facing > 0 ? lit(wx + nx * 0.08, wy + (face === TOP ? 0.02 : 0), wz + nz * 0.08) : 0;
    shadow *= shadow + (1 - shadow) * soft;
    const direct = shadow * facing;
    const sky2 = sky * sky;
    const lamp2 = lamp * lamp;
    const lamp4 = lamp2 * lamp2;
    const curve = lamp4 * lamp4 * lamp2 * 1.6 + lamp * 0.6;
    const hue = shine > 0 ? 1 / (Math.sqrt(r * r + g * g + b * b) + 0.00001) : 0;
    // A bright texel is held back a little, so pale blocks do not burn out.
    const balance = 1 / (1 + Math.max(r, g, b) * 0.25 * (1 - flatten));
    const faceShade = (plant ? 1 : DIFFUSE[face]!) * (1 - flatten) + flatten;
    const cornerShade = corner * (1 - flatten) + flatten;
    const fall = balance * faceShade * cornerShade * cornerShade;
    const boost = sky2 * (1 + through * shadow);
    const red =
      (ambientColor[0]! * sky + (lightColor[0]! - ambientColor[0]! * sky) * direct) * boost +
      lampColor[0] * curve * curve +
      MIN_LIGHT * (1 - sky2) +
      (r * hue + (1 - r * hue) * shine * 0.5) * shine * 4;
    const green =
      (ambientColor[1]! * sky + (lightColor[1]! - ambientColor[1]! * sky) * direct) * boost +
      lampColor[1] * curve * curve +
      MIN_LIGHT * (1 - sky2) +
      (g * hue + (1 - g * hue) * shine * 0.5) * shine * 4;
    const blue =
      (ambientColor[2]! * sky + (lightColor[2]! - ambientColor[2]! * sky) * direct) * boost +
      lampColor[2] * curve * curve +
      MIN_LIGHT * (1 - sky2) +
      (b * hue + (1 - b * hue) * shine * 0.5) * shine * 4;
    r *= red * fall;
    g *= green * fall;
    b *= blue * fall;
    // Away from the sun and from any lamp, colour drains toward the night's blue.
    const away = (1 - lamp) * (1 - lamp);
    const drained = (1 - Math.sqrt(sky) * daylight) * clamp01((away - 0.25) / 0.75) ** 2 * (1 - flatten);
    if (drained > 0.001) {
      const grey = (r * 0.2126 + g * 0.7152 + b * 0.0722) * 1.7;
      const root = Math.sqrt(sky);
      const k = drained * 0.4;
      r += (grey * (0.4 + (nightHue[0]! - 0.4) * root) - r) * k;
      g += (grey * (0.4 + (nightHue[1]! - 0.4) * root) - g) * k;
      b += (grey * (0.4 + (nightHue[2]! - 0.4) * root) - b) * k;
    }

    const mist = haze(depth);
    out[o] += (r + (fogR - r) * mist) * share;
    out[o + 1] += (g + (fogG - g) * mist) * share;
    out[o + 2] += (b + (fogB - b) * mist) * share;
    return true;
  };

  const drawColumn = (x: number): void => {
    col = x;
    scatter = Math.imul(x + 1, 0x9e3779b1);
    const u = (x + 0.5 - W / 2) / focal;
    angle = Math.atan(u);
    dirX = fwdX + fwdZ * u;
    dirZ = fwdZ - fwdX * u;
    span = 1 + u * u;
    // Looking toward the light, leaves and grass glow with what passes through them.
    glare = (((dirX * lightX + dirZ * lightZ) / Math.sqrt(span)) * 0.5 + 0.5) ** 16;
    // Where this column's line leaves the grid: the edge of the world, which the air hides.
    const edgeX = dirX > 0 ? (n - 1 - cx) / dirX : dirX < 0 ? (1 - cx) / dirX : FAR;
    const edgeZ = dirZ > 0 ? (n - 1 - cz) / dirZ : dirZ < 0 ? (1 - cz) / dirZ : FAR;
    reach = Math.min(FAR, edgeX, edgeZ);
    // The air takes the colour of the sky at the horizon that way.
    skyColor(skyTap, 0, (dirX * sunX + dirZ * sunZ) / Math.sqrt(span));
    fogR = skyTap[0]!;
    fogG = skyTap[1]!;
    fogB = skyTap[2]!;
    left.fill(1);
    lo = 0;
    hi = H;
    const base = x * 3;
    const stride = W * 3;

    // Walk the grid along this column's line, keeping the path for the mirror pass.
    let ix = Math.floor(cx + dirX * NEAR);
    let iz = Math.floor(cz + dirZ * NEAR);
    const stepX = dirX > 0 ? 1 : -1;
    const stepZ = dirZ > 0 ? 1 : -1;
    const deltaX = dirX !== 0 ? Math.abs(1 / dirX) : Infinity;
    const deltaZ = dirZ !== 0 ? Math.abs(1 / dirZ) : Infinity;
    let nextX = dirX !== 0 ? (ix + (dirX > 0 ? 1 : 0) - cx) / dirX : Infinity;
    let nextZ = dirZ !== 0 ? (iz + (dirZ > 0 ? 1 : 0) - cz) / dirZ : Infinity;
    let enter = NEAR;
    let face = SIDE_Z;
    let steps = 0;
    let foot = 0;
    let waterLo = H;
    let waterHi = 0;
    // The foliage the ray just came through, so leaves behind leaves read as the inside of a crown.
    let leafLo = 0;
    let leafHi = 0;

    /**
     * Takes a pixel's share of a span that runs from screen height `top` to `bottom`: all of what is left of the
     * pixel inside the span, and only the covered fraction on the rows where the span begins and ends.
     */
    const take = (row: number, top: number, bottom: number): number => {
      const cover = (row + 1 < bottom ? row + 1 : bottom) - (row > top ? row : top);
      const rest = left[row]!;
      return cover < rest ? (cover > 0.004 ? cover : 0) : rest;
    };
    const took = (row: number, share: number, depth: number): void => {
      const rest = left[row]! - share;
      left[row] = rest < 0.004 ? 0 : rest;
      if (share >= 0.5 || depths[row * W + x] === 0) depths[row * W + x] = depth;
    };

    while (ix >= 0 && iz >= 0 && ix < n && iz < n && enter < FAR && lo < hi) {
      const exit = Math.min(nextX, nextZ);
      const ci = iz * n + ix;
      pathCell[steps] = ci;
      pathEnter[steps] = enter;
      pathExit[steps] = exit;
      pathFace[steps] = face;
      steps++;

      const g = ground[ci]!;
      const wet = hasWater && g < world.waterLevel;
      const groundTop = wet ? surface : groundMat[ci] === LAVA ? g - 0.12 : g;
      const scaleIn = focal / enter;
      const scaleOut = focal / exit;
      const texelIn = scaleIn / 16;
      const count = runCount[ci]!;
      const wx = cx + dirX * enter;
      const wz = cz + dirZ * enter;
      let nextLeafLo = 0;
      let nextLeafHi = 0;
      let plantAt = -1;

      // Faces toward the camera first: they hide the tops behind them.
      for (let k = -1; k < count; k++) {
        let y0 = 0;
        let y1 = groundTop;
        let mat = groundMat[ci]!;
        if (k >= 0) {
          const ro = (ci * RUNS + k) * 3;
          y0 = runs[ro]!;
          y1 = runs[ro + 1]!;
          mat = runs[ro + 2]!;
          if (mat >= PLANT) {
            plantAt = ro;
            continue;
          }
          if (mat >= FOLIAGE && mat < LAVA) {
            if (nextLeafHi === 0) nextLeafLo = y0;
            nextLeafHi = y1;
          }
        }
        // Of the ground's side only what stands above the column in front is exposed.
        if (k < 0 && (y1 <= foot || (y0 = foot) < 0)) continue;
        const top = hz - (y1 - cy) * scaleIn;
        const bottom = hz - (y0 - cy) * scaleIn;
        const to = Math.min(hi, Math.ceil(bottom));
        for (let row = Math.max(lo, Math.floor(top)); row < to; row++) {
          const share = take(row, top, bottom);
          if (share === 0) continue;
          const at = ((row > top ? row : top) + (row + 1 < bottom ? row + 1 : bottom)) / 2;
          const wy = cy - (at - hz) / scaleIn;
          const inner = wy > leafLo && wy < leafHi ? 0.55 : 1;
          if (
            shade(
              hdr,
              row * stride + base,
              mat,
              face,
              wx,
              wy,
              wz,
              enter,
              texelIn,
              k < 0 ? g : -1,
              foot,
              inner,
              -1,
              share,
            )
          )
            took(row, share, enter);
        }
      }
      // A plant is two cut-outs crossing the cell corner to corner; the ray meets each once.
      if (plantAt >= 0 && texelIn > 0.16) {
        const sumD = dirX + dirZ;
        const difD = dirX - dirZ;
        const tA = difD !== 0 ? (ix - cx - (iz - cz)) / difD : -1;
        const tB = sumD !== 0 ? (1 + ix + iz - cx - cz) / sumD : -1;
        for (let pass = 0; pass < 2; pass++) {
          const t = pass === 0 ? Math.min(tA, tB) : Math.max(tA, tB);
          if (t <= enter || t >= exit) continue;
          const px = cx + dirX * t;
          const across = px - ix;
          if (across < 0 || across > 1) continue;
          const pz = cz + dirZ * t;
          const scale = focal / t;
          const y0 = runs[plantAt]!;
          const y1 = runs[plantAt + 1]!;
          const top = hz - (y1 - cy) * scale;
          const bottom = hz - (y0 - cy) * scale;
          const to = Math.min(hi, Math.ceil(bottom));
          for (let row = Math.max(lo, Math.floor(top)); row < to; row++) {
            const share = take(row, top, bottom);
            if (share === 0) continue;
            const at = ((row > top ? row : top) + (row + 1 < bottom ? row + 1 : bottom)) / 2;
            const wy = Math.min(y1 - 0.001, Math.max(y0, cy - (at - hz) / scale));
            if (
              shade(
                hdr,
                row * stride + base,
                runs[plantAt + 2]!,
                SIDE_Z,
                px,
                wy,
                pz,
                t,
                scale / 16,
                -1,
                y0,
                1,
                across,
                share,
              )
            )
              took(row, share, t);
          }
        }
      }
      // Then tops seen from above and undersides seen from below.
      for (let k = -1; k < count; k++) {
        let y0 = 0;
        let y1 = groundTop;
        let mat = wet ? WATER : groundMat[ci]!;
        if (k >= 0) {
          const ro = (ci * RUNS + k) * 3;
          y0 = runs[ro]!;
          y1 = runs[ro + 1]!;
          mat = runs[ro + 2]!;
          if (mat >= PLANT || mat === VINE) continue;
        }
        const over = k < 0 ? decal[ci]! : 0;
        const above = cy > y1;
        if (!above && (k < 0 || cy >= y0)) continue;
        // The face's far edge and near edge on screen: a top lies below the horizon, an underside above it.
        const rise = above ? cy - y1 : cy - y0;
        const top = hz + rise * (above ? scaleOut : scaleIn);
        const bottom = hz + rise * (above ? scaleIn : scaleOut);
        const to = Math.min(hi, Math.ceil(bottom));
        for (let row = Math.max(lo, Math.floor(top)); row < to; row++) {
          const share = take(row, top, bottom);
          if (share === 0) continue;
          const at = ((row > top ? row : top) + (row + 1 < bottom ? row + 1 : bottom)) / 2;
          const depth = Math.min(exit, Math.max(enter, (rise * focal) / (at - hz)));
          // A ground plane seen at a slant stretches each pixel over more texels along the view than across it.
          const texel = Math.sqrt((focal / depth) * ((Math.abs(rise) * focal) / (depth * depth))) / 16;
          const wpx = cx + dirX * depth;
          const wpz = cz + dirZ * depth;
          // What lies on the surface hides it where its texture is not a gap.
          if (
            over !== 0 &&
            above &&
            shade(hdr, row * stride + base, over, TOP, wpx, y1, wpz, depth, texel, -1, 0, 1, -1, share)
          ) {
            took(row, share, depth);
            continue;
          }
          if (mat === WATER) {
            took(row, share, depth);
            waterShare[row] += share;
            waterDepth[row] = depth;
            waterCell[row] = ci;
            if (row < waterLo) waterLo = row;
            if (row >= waterHi) waterHi = row + 1;
            continue;
          }
          if (
            shade(
              hdr,
              row * stride + base,
              mat,
              above ? TOP : UNDER,
              wpx,
              above ? y1 : y0,
              wpz,
              depth,
              texel,
              -1,
              0,
              above ? 1 : 0.8,
              -1,
              share,
            )
          )
            took(row, share, depth);
        }
      }
      // Foliage is seen from inside too: the far faces of a leafy block close the gaps of its near ones.
      for (let k = nextLeafHi === 0 ? count : 0; k < count; k++) {
        const ro = (ci * RUNS + k) * 3;
        const mat = runs[ro + 2]!;
        if (mat < FOLIAGE || mat >= LAVA) continue;
        const y0 = runs[ro]!;
        const y1 = runs[ro + 1]!;
        // Its far side, then its top from below or its underside from above.
        for (let pass = 0; pass < 2; pass++) {
          if (pass === 1 && (mat === VINE || (cy >= y0 && cy <= y1))) break;
          const rise = cy - (cy < y0 ? y1 : y0);
          const a = pass === 0 ? hz - (y1 - cy) * scaleOut : hz + rise * scaleIn;
          const b = pass === 0 ? hz - (y0 - cy) * scaleOut : hz + rise * scaleOut;
          const top = a < b ? a : b;
          const bottom = a < b ? b : a;
          const to = Math.min(hi, Math.ceil(bottom));
          for (let row = Math.max(lo, Math.floor(top)); row < to; row++) {
            const share = take(row, top, bottom);
            if (share === 0) continue;
            const at = ((row > top ? row : top) + (row + 1 < bottom ? row + 1 : bottom)) / 2;
            const depth = pass === 0 ? exit : Math.min(exit, Math.max(enter, (rise * focal) / (at - hz)));
            const wy = pass === 0 ? cy - (at - hz) / scaleOut : cy - rise;
            const side = pass === 1 ? UNDER : nextX < nextZ ? SIDE_X : SIDE_Z;
            const o = row * stride + base;
            if (
              shade(
                hdr,
                o,
                mat,
                side,
                cx + dirX * depth,
                wy,
                cz + dirZ * depth,
                depth,
                focal / depth / 16,
                -1,
                0,
                0.55,
                -1,
                share,
              )
            )
              took(row, share, depth);
          }
        }
      }
      while (lo < hi && left[lo] === 0) lo++;
      while (hi > lo && left[hi - 1] === 0) hi--;

      leafLo = nextLeafLo;
      leafHi = nextLeafHi;
      foot = groundTop;
      enter = exit;
      if (nextX < nextZ) {
        nextX += deltaX;
        ix += stepX;
        face = SIDE_X;
      } else {
        nextZ += deltaZ;
        iz += stepZ;
        face = SIDE_Z;
      }
    }
    // Open water runs on past the edge of the grid to the horizon.
    if (hasWater && steps > 0 && lo < hi && ground[pathCell[steps - 1]!]! < world.waterLevel) {
      for (let row = Math.max(lo, Math.ceil(hz)); row < hi; row++) {
        const share = left[row]!;
        if (share === 0) continue;
        const depth = Math.min(FAR * 3, ((cy - surface) * focal) / (row + 0.5 - hz));
        took(row, share, depth);
        waterShare[row] += share;
        waterDepth[row] = depth;
        waterCell[row] = pathCell[steps - 1]!;
        if (row < waterLo) waterLo = row;
        if (row >= waterHi) waterHi = row + 1;
      }
    }
    for (let row = lo; row < hi; row++) {
      const share = left[row]!;
      if (share === 0) continue;
      skyInto(skyTap, 0, row, true);
      const o = row * stride + base;
      hdr[o] += skyTap[0]! * share;
      hdr[o + 1] += skyTap[1]! * share;
      hdr[o + 2] += skyTap[2]! * share;
      if (share >= 0.5 || depths[row * W + x] === 0) depths[row * W + x] = FAR * 4;
    }

    if (waterLo >= waterHi) return;
    // The same walk again with the world flipped about the water, into the water's pixels only.
    const m = mirror!;
    mirrored.fill(0, waterLo, waterHi);
    m.fill(0, waterLo * 3, waterHi * 3);
    let todo = 0;
    for (let row = waterLo; row < waterHi; row++) if (waterDepth[row] !== 0) todo++;
    const flip = cy - 2 * surface;
    for (let s = 0; s < steps && todo > 0; s++) {
      const ci = pathCell[s]!;
      const g = ground[ci]!;
      const count = runCount[ci]!;
      if (g <= surface && count === 0) continue;
      const scaleIn = focal / pathEnter[s]!;
      const scaleOut = focal / pathExit[s]!;
      const wx = cx + dirX * pathEnter[s]!;
      const wz = cz + dirZ * pathEnter[s]!;
      for (let k = -1; k < count; k++) {
        let y0 = surface;
        let y1 = g;
        let mat = groundMat[ci]!;
        if (k >= 0) {
          const ro = (ci * RUNS + k) * 3;
          y0 = Math.max(surface, runs[ro]!);
          y1 = runs[ro + 1]!;
          mat = runs[ro + 2]!;
          if (mat >= PLANT) continue;
        }
        if (y1 <= surface) continue;
        const from = Math.max(waterLo, Math.ceil(hz + (flip + y0) * scaleIn - 0.5));
        const to = Math.min(waterHi, Math.ceil(hz + (flip + y1) * scaleIn - 0.5));
        for (let row = from; row < to; row++) {
          if (mirrored[row] || waterDepth[row] === 0) continue;
          const wy = (row + 0.5 - hz) / scaleIn - flip;
          if (
            !shade(
              m,
              row * 3,
              mat,
              pathFace[s]!,
              wx,
              wy,
              wz,
              pathEnter[s]!,
              scaleIn / 16,
              k < 0 ? g : -1,
              surface,
              1,
              -1,
              1,
            )
          )
            continue;
          mirrored[row] = 1;
          todo--;
        }
        if (k >= 0 && y0 > surface) {
          const under = Math.max(waterLo, Math.ceil(hz + (flip + y0) * scaleOut - 0.5));
          const until = Math.min(waterHi, Math.ceil(hz + (flip + y0) * scaleIn - 0.5));
          for (let row = under; row < until; row++) {
            if (mirrored[row] || waterDepth[row] === 0) continue;
            const depth = ((flip + y0) * focal) / (row + 0.5 - hz);
            if (
              !shade(
                m,
                row * 3,
                mat,
                UNDER,
                cx + dirX * depth,
                y0,
                cz + dirZ * depth,
                depth,
                scaleIn / 32,
                -1,
                0,
                0.8,
                -1,
                1,
              )
            )
              continue;
            mirrored[row] = 1;
            todo--;
          }
        }
      }
    }
    for (let row = waterLo; row < waterHi; row++) {
      if (!mirrored[row] && waterDepth[row] !== 0) skyInto(m, row * 3, 2 * hz - row - 1, false);
    }
    // Water: the swell tilts the surface, which bends the reflection, opens or closes the view of the bed, and
    // catches the sun as a broken path of light.
    const eye = cy - surface;
    for (let row = waterLo; row < waterHi; row++) {
      const depth = waterDepth[row]!;
      if (depth === 0) continue;
      const wx = cx + dirX * depth;
      const wz = cz + dirZ * depth;
      swell(wx, wz, (depth * depth) / (focal * eye));
      const tilt = 1 / Math.sqrt(1 + slopeX * slopeX + slopeZ * slopeZ);
      const nx = -slopeX * tilt;
      const nz = -slopeZ * tilt;
      const vx = dirX * depth;
      const vz = dirZ * depth;
      const reach = 1 / Math.sqrt(vx * vx + eye * eye + vz * vz);
      const facing = (eye * tilt - vx * nx - vz * nz) * reach;
      const cos = facing < 0.02 ? 0.02 : facing;
      // The mirrored ray, and how high above the horizon it leaves.
      const rx = vx * reach + 2 * cos * nx;
      const ry = 2 * cos * tilt - eye * reach;
      const rz = vz * reach + 2 * cos * nz;
      const climb = Math.max(0.004, ry / Math.hypot(rx, rz));
      const from = Math.round(hz + climb * focal);
      const o = row * stride + base;
      if (from >= waterLo && from < waterHi && waterDepth[from] !== 0) skyTap.set(m.subarray(from * 3, from * 3 + 3));
      else skyInto(skyTap, 0, hz - climb * focal, false);
      // Waves too fine to see still scatter the light: the rougher the water, the wider and fainter its glitter.
      const sunlit = lit(wx, surface + 0.05, wz);
      const toLight = rx * lightX + ry * lightY + rz * lightZ;
      const gloss = 1 / (0.008 + chop * 2.5);
      const sparkle = toLight > 0.5 ? toLight ** gloss * (0.5 + gloss * 0.018) * sunlit : 0;
      const grazing = 1 - cos;
      const fresnel = 0.02 + 0.98 * grazing * grazing * grazing * grazing * grazing;
      // The bed, lit and textured like any ground, seen where the bent ray lands on it and dimmed by the water
      // over it. Its depth is smoothed between columns: water shows no squares.
      const ci = waterCell[row]!;
      const gx = Math.min(n - 1.001, Math.max(0, wx - 0.5));
      const gz = Math.min(n - 1.001, Math.max(0, wz - 0.5));
      const g0 = Math.floor(gz) * n + Math.floor(gx);
      const fx = gx - Math.floor(gx);
      const fz = gz - Math.floor(gz);
      const bed =
        (ground[g0]! * (1 - fx) + ground[g0 + 1]! * fx) * (1 - fz) +
        (ground[g0 + n]! * (1 - fx) + ground[g0 + n + 1]! * fx) * fz;
      const thick = Math.max(0, surface - bed);
      const path = thick / Math.sqrt(0.44 + 0.56 * cos * cos);
      const texel = Math.sqrt((focal / depth) * ((eye * focal) / (depth * depth))) / 16;
      bedTap.fill(0);
      shade(
        bedTap,
        0,
        groundMat[ci]!,
        TOP,
        wx + nx * thick,
        ground[ci]!,
        wz + nz * thick,
        depth,
        texel,
        -1,
        -1,
        1,
        -1,
        1,
      );
      // Water as shader packs draw it: one deep blue laid over the bed at seven parts in ten, the bed dimming
      // further with the water over it, and over both the mirror, strongest at a glancing angle.
      const mist = haze(depth);
      const murk = 1 - Math.exp(-path / 2.5);
      const direct = sunlit * (0.7 + 0.3 * lightY);
      for (let c = 0; c < 3; c++) {
        const lightC = ambientColor[c]! + (lightColor[c]! - ambientColor[c]!) * direct;
        const own = waterColor[c]! * 0.1225 * lightC;
        const under = bedTap[c]! + (waterColor[c]! * 0.18 * (lightColor[c]! * 0.9 + 0.1) - bedTap[c]!) * murk;
        const body = own * 0.7 + under * 0.3;
        const shown = body + (skyTap[c]! - body) * fresnel + lightColor[c]! * sparkle;
        hdr[o + c] += (shown + ((c === 0 ? fogR : c === 1 ? fogG : fogB) - shown) * mist) * waterShare[row]!;
      }
    }
    waterDepth.fill(0, waterLo, waterHi);
    waterShare.fill(0, waterLo, waterHi);
  };

  return {
    width: W,
    height: H,
    columns(from, to) {
      for (let x = from; x < to; x++) drawColumn(x);
    },
    finish() {
      const steps = this.finishing();
      for (;;) {
        const step = steps.next();
        if (step.done) return step.value;
      }
    },
    *finishing() {
      // Edges that run up the picture fall exactly between columns, hard but never stepped. Soften them a little
      // where something near ends against something far; span ends were already given their exact coverage.
      const line = new Float32Array(W * 3);
      for (let y = 0; y < H; y++) {
        line.set(hdr.subarray(y * W * 3, (y + 1) * W * 3));
        for (let x = 1; x < W - 1; x++) {
          const i = y * W + x;
          const d = depths[i]!;
          const gap = Math.max(Math.abs(depths[i - 1]! - d), Math.abs(depths[i + 1]! - d));
          if (gap < d * 0.12 + 0.3) continue;
          for (let c = 0; c < 3; c++) {
            hdr[i * 3 + c] = line[x * 3 + c]! * 0.6 + (line[(x - 1) * 3 + c]! + line[(x + 1) * 3 + c]!) * 0.2;
          }
        }
      }
      yield;
      // Bloom: the whole picture blurred at quarter size, to be laid back over itself at a fifth.
      const soft = new Float32Array(qw * qh * 3);
      for (let y = 0; y < H; y++) {
        for (let x = 0; x < W; x++) {
          const o = (y * W + x) * 3;
          const b = ((y >> 2) * qw + (x >> 2)) * 3;
          soft[b] += hdr[o]! / 16;
          soft[b + 1] += hdr[o + 1]! / 16;
          soft[b + 2] += hdr[o + 2]! / 16;
        }
      }
      const scratch = new Float32Array(soft.length);
      const blur = (src: Float32Array, dst: Float32Array, step: number, radius: number): void => {
        const length = step === 3 ? qw : qh;
        const lines = step === 3 ? qh : qw;
        const lineStride = step === 3 ? qw * 3 : 3;
        for (let row = 0; row < lines; row++) {
          for (let c = 0; c < 3; c++) {
            const start = row * lineStride + c;
            let sum = 0;
            for (let i = -radius; i <= radius; i++) sum += src[start + Math.min(length - 1, Math.max(0, i)) * step]!;
            for (let i = 0; i < length; i++) {
              dst[start + i * step] = sum / (radius * 2 + 1);
              sum +=
                src[start + Math.min(length - 1, i + radius + 1) * step]! -
                src[start + Math.max(0, i - radius) * step]!;
            }
          }
        }
      };
      for (const radius of [1, 2, 5]) {
        blur(soft, scratch, 3, radius);
        blur(scratch, soft, qw * 3, radius);
      }

      yield;
      // Light shafts: open sky near the light, smeared away from it, so trunks and crowns cut dark gaps in the haze.
      const shafts = new Float32Array(qw * qh);
      if (lightAhead && lightY > 0) {
        const sx = (W / 2 + Math.tan(lightAzimuth) * focal) / 4;
        const sy = (hz - lightRise * focal) / 4;
        const taps = 24;
        for (let y = 0; y < qh; y++) {
          for (let x = 0; x < qw; x++) {
            const stepX = ((sx - x) / taps) * 0.95;
            const stepY = ((sy - y) / taps) * 0.95;
            let sum = 0;
            let weight = 1;
            for (let k = 0; k < taps; k++) {
              const px = Math.round(x + stepX * k);
              const py = Math.round(y + stepY * k);
              if (px >= 0 && py >= 0 && px < qw && py < qh) sum += openSky[py * qw + px]! * weight;
              weight *= 0.95;
            }
            shafts[y * qw + x] = (sum / taps) * rays * 0.08;
          }
        }
      }

      yield;
      // The shader packs' tone curve: brighten fourfold, let the brightest colours drift toward white, roll off
      // smoothly, then to display values.
      const curve = new Float32Array(4097);
      for (let i = 0; i <= 4096; i++) {
        const value = i / 256;
        curve[i] = value / Math.sqrt(value * value + 1);
      }
      const display = new Uint8Array(1025);
      for (let i = 0; i <= 1024; i++) display[i] = Math.round(255 * (i / 1024) ** (1 / 2.2));
      // The inverse of the drift toward white, which gives the colour back after the roll-off.
      const [p, q, s] = [1.0428, -0.033, -0.0098];
      const mid = 1.0659;
      const out = new Uint8ClampedArray(W * H * 4);
      for (let y = 0; y < H; y++) {
        if ((y & 63) === 63) yield;
        const gy = Math.max(0, (y - 1.5) / 4);
        const y0 = Math.min(qh - 1, gy | 0);
        const y1 = Math.min(qh - 1, y0 + 1);
        const fy = gy - y0;
        for (let x = 0; x < W; x++) {
          const gx = Math.max(0, (x - 1.5) / 4);
          const x0 = Math.min(qw - 1, gx | 0);
          const x1 = Math.min(qw - 1, x0 + 1);
          const fx = gx - x0;
          const a = y0 * qw + x0;
          const b = y0 * qw + x1;
          const c = y1 * qw + x0;
          const d = y1 * qw + x1;
          const o = (y * W + x) * 3;
          const shaft =
            shafts[a]! +
            (shafts[b]! - shafts[a]!) * fx +
            (shafts[c]! + (shafts[d]! - shafts[c]!) * fx - shafts[a]! - (shafts[b]! - shafts[a]!) * fx) * fy;
          let r = 0;
          let g = 0;
          let bl = 0;
          for (let ch = 0; ch < 3; ch++) {
            const upper = soft[a * 3 + ch]! + (soft[b * 3 + ch]! - soft[a * 3 + ch]!) * fx;
            const lower = soft[c * 3 + ch]! + (soft[d * 3 + ch]! - soft[c * 3 + ch]!) * fx;
            const value = (hdr[o + ch]! * 0.8 + (upper + (lower - upper) * fy) * 0.2 + lightColor[ch]! * shaft) * 4;
            if (ch === 0) r = value;
            else if (ch === 1) g = value;
            else bl = value;
          }
          const tr = curve[Math.min(4096, ((r * 0.96 + g * 0.03 + bl * 0.01) * 256) | 0)]!;
          const tg = curve[Math.min(4096, ((r * 0.03 + g * 0.94 + bl * 0.03) * 256) | 0)]!;
          const tb = curve[Math.min(4096, ((r * 0.01 + g * 0.03 + bl * 0.96) * 256) | 0)]!;
          const noise = (hash3(x, y, 5) - 0.5) * 1.6;
          const pxl = (y * W + x) * 4;
          out[pxl] = display[(clamp01(tr * p + tg * q + tb * s) * 1024) | 0]! + noise;
          out[pxl + 1] = display[(clamp01(tr * q + tg * mid + tb * q) * 1024) | 0]! + noise;
          out[pxl + 2] = display[(clamp01(tr * s + tg * q + tb * p) * 1024) | 0]! + noise;
          out[pxl + 3] = 255;
        }
      }
      return out;
    },
  };
}
