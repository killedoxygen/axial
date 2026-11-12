import { finishing } from './finish';
import { Light, Shadows, Tops, clamp01, hash3 } from './light';
import { Sky, type Ray, type View } from './sky';
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

  const light = new Light(world);
  const {
    daylight,
    color: lightColor,
    ambient: ambientColor,
    lamp: lampColor,
    nightHue,
    water: waterColor,
    x: lightX,
    y: lightY,
    z: lightZ,
  } = light;
  const MIN_LIGHT = 0.063;
  const view: View = { width: W, height: H, focal, horizon: hz, x: cx, y: cy, z: cz, yaw: camera.yaw };
  const ray: Ray = { col: 0, dirX: 0, dirZ: 0, span: 1, angle: 0 };
  const sky = new Sky(world, light, view, ray);
  const tops = new Tops(world);
  const { solid: solidTop, leaf: leafTop, rim } = tops;
  const shadows = new Shadows(world, light, tops);

  const hdr = new Float32Array(W * H * 3);
  // Depth per pixel, so edges between near and far can be smoothed without softening the textures.
  const depths = new Float32Array(W * H);
  const mirror = hasWater ? new Float32Array(H * 3) : null;
  const waterDepth = new Float32Array(H);
  const waterCell = new Int32Array(H);
  // Rock is not one stone: veins of andesite and granite run through it, and coal shows here and there, in a
  // version that has them. One that does not is plain stone throughout.
  const known = (tex: number): number => (atlas[tex * TEXELS * 4 + 3]! > 0 ? tex : STONE_TEX);
  const andesite = known(ANDESITE_TEX);
  const granite = known(GRANITE_TEX);
  const coal = known(COAL_TEX);
  const veins = new Uint8Array(n * n);
  for (let z = 0; z < n; z++) {
    for (let x = 0; x < n; x++) {
      const vein = valueNoise(x / 7, z / 7, world.seed + 201);
      veins[z * n + x] = vein > 0.74 ? andesite : vein < 0.2 ? granite : STONE_TEX;
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
  // How much of a far thing the air hides: a thin haze that thickens at night, tabled by distance.
  const hazes = new Float32Array(FAR + 2);
  for (let depth = 0; depth < hazes.length; depth++) {
    let fog = (depth / 1024) * (2.5 - 1.5 * daylight);
    if (fog > 0.5) fog = (fog - 0.5) * 0.25 + 0.5;
    hazes[depth] = 1 - Math.exp(-2 * fog ** (0.35 * daylight + 1.25));
  }
  /** The haze at a distance, then the edge of the world, where everything gives way to the sky. */
  const haze = (depth: number): number => {
    const fog = hazes[depth > FAR ? FAR : depth | 0]!;
    const edge = 1 - (reach - depth) / (0.2 * reach);
    return edge > 0 ? fog + (1 - fog) * (edge > 1 ? 1 : edge) : fog;
  };

  /** How dark an inner corner is where two sides and the block diagonally between them meet, as the game has it. */
  const meet = (first: number, second: number, blocked: boolean): number =>
    (1 + first + second + (first + second < 1 || blocked ? 0.2 : 1)) / 4;
  /**
   * The light on a surface in one colour: the sky's and the sun's or moon's, a lamp's by the packs' curve, the
   * least the game allows, and the surface's own glow in its own hue.
   */
  const lightOn = (
    c: number,
    value: number,
    sky: number,
    direct: number,
    boost: number,
    curve: number,
    floor: number,
    hue: number,
    shine: number,
  ): number =>
    (ambientColor[c]! * sky + (lightColor[c]! - ambientColor[c]! * sky) * direct) * boost +
    lampColor[c]! * curve * curve +
    floor +
    (value * hue + (1 - value * hue) * shine * 0.5) * shine * 2.2;
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
      tex = hash3(bx, by, bz) > 0.965 ? coal : veins[((bz + shift * 7) % n) * n + ((bx + shift * 5) % n)]!;
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
          const px = wx - bx;
          const pz = wz - bz;
          corner *=
            (meet(west, north, solidTop[fi - n - 1]! > high) * (1 - px) +
              meet(east, north, solidTop[fi - n + 1]! > high) * px) *
              (1 - pz) +
            (meet(west, south, solidTop[fi + n - 1]! > high) * (1 - px) +
              meet(east, south, solidTop[fi + n + 1]! > high) * px) *
              pz;
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
    let shadow = facing > 0 ? shadows.lit(wx + nx * 0.08, wy + (face === TOP ? 0.02 : 0), wz + nz * 0.08) : 0;
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
    const floor = MIN_LIGHT * (1 - sky2);
    const red = lightOn(0, r, sky, direct, boost, curve, floor, hue, shine);
    const green = lightOn(1, g, sky, direct, boost, curve, floor, hue, shine);
    const blue = lightOn(2, b, sky, direct, boost, curve, floor, hue, shine);
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

  // The column being drawn, and the span of it being filled: what the span is made of and where it lies.
  const stride = W * 3;
  let column = 0;
  let base = 0;
  let enter = NEAR;
  let exit = NEAR;
  let waterLo = H;
  let waterHi = 0;
  // The foliage the ray just came through, so leaves behind leaves read as the inside of a crown.
  let leafLo = 0;
  let leafHi = 0;
  let sMat = 0;
  let sFace = 0;
  let sX = 0;
  let sZ = 0;
  let sDepth = 0;
  let sScale = 0;
  let sLip = -1;
  let sFoot = 0;
  let sInner = 1;
  let sAcross = -1;
  let sLow = 0;
  let sHigh = 0;
  let sRise = 0;
  let sOver = 0;
  let sCell = 0;
  let sSlant = false;

  /**
   * Fills the rows a span covers from screen height `top` to `bottom`. Each row takes all of what is left of its
   * pixel inside the span, and only the covered fraction where the span begins and ends; `point` shades the
   * middle of what the row covers and returns its depth, or zero where the texture is a gap.
   */
  const fill = (top: number, bottom: number, point: (at: number, row: number, share: number) => number): void => {
    const to = Math.min(hi, Math.ceil(bottom));
    for (let row = Math.max(lo, Math.floor(top)); row < to; row++) {
      const upper = row > top ? row : top;
      const lower = row + 1 < bottom ? row + 1 : bottom;
      const cover = lower - upper;
      const rest = left[row]!;
      const share = cover < rest ? (cover > 0.004 ? cover : 0) : rest;
      if (share === 0) continue;
      const depth = point((upper + lower) / 2, row, share);
      if (depth === 0) continue;
      left[row] = rest - share < 0.004 ? 0 : rest - share;
      if (share >= 0.5 || depths[row * W + column] === 0) depths[row * W + column] = depth;
    }
  };
  /** A point on an upright face of a block. A negative `sInner` asks whether foliage stands in front of it. */
  const wall = (at: number, row: number, share: number): number => {
    const wy = cy - (at - hz) / sScale;
    const inner = sInner < 0 ? (wy > leafLo && wy < leafHi ? 0.55 : 1) : sInner;
    return shade(hdr, row * stride + base, sMat, sFace, sX, wy, sZ, sDepth, sScale / 16, sLip, sFoot, inner, -1, share)
      ? sDepth
      : 0;
  };
  /** A point on one of a plant's two cut-outs. */
  const sprite = (at: number, row: number, share: number): number => {
    const wy = Math.min(sHigh - 0.001, Math.max(sLow, cy - (at - hz) / sScale));
    return shade(hdr, row * stride + base, sMat, SIDE_Z, sX, wy, sZ, sDepth, sScale / 16, -1, sLow, 1, sAcross, share)
      ? sDepth
      : 0;
  };
  /** A point on a top seen from above or an underside seen from below; open water is only noted, for the mirror. */
  const level = (at: number, row: number, share: number): number => {
    const depth = Math.min(exit, Math.max(enter, (sRise * focal) / (at - hz)));
    // A ground plane seen at a slant stretches each pixel over more texels along the view than across it.
    const texel = sSlant
      ? Math.sqrt((focal / depth) * ((Math.abs(sRise) * focal) / (depth * depth))) / 16
      : focal / depth / 16;
    const wpx = cx + dirX * depth;
    const wpz = cz + dirZ * depth;
    const o = row * stride + base;
    // What lies on the surface hides it where its texture is not a gap.
    if (sOver !== 0 && shade(hdr, o, sOver, TOP, wpx, sHigh, wpz, depth, texel, -1, 0, 1, -1, share)) return depth;
    if (sMat === WATER) {
      waterShare[row] += share;
      waterDepth[row] = depth;
      waterCell[row] = sCell;
      if (row < waterLo) waterLo = row;
      if (row >= waterHi) waterHi = row + 1;
      return depth;
    }
    return shade(hdr, o, sMat, sFace, wpx, sHigh, wpz, depth, texel, -1, 0, sInner, -1, share) ? depth : 0;
  };

  const drawColumn = (x: number): void => {
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
    ray.col = x;
    ray.dirX = dirX;
    ray.dirZ = dirZ;
    ray.span = span;
    ray.angle = angle;
    sky.color(skyTap, 0, (dirX * light.sun[0] + dirZ * light.sun[2]) / Math.sqrt(span));
    fogR = skyTap[0]!;
    fogG = skyTap[1]!;
    fogB = skyTap[2]!;
    left.fill(1);
    lo = 0;
    hi = H;
    column = x;
    base = x * 3;

    // Walk the grid along this column's line, keeping the path for the mirror pass.
    let ix = Math.floor(cx + dirX * NEAR);
    let iz = Math.floor(cz + dirZ * NEAR);
    const stepX = dirX > 0 ? 1 : -1;
    const stepZ = dirZ > 0 ? 1 : -1;
    const deltaX = dirX !== 0 ? Math.abs(1 / dirX) : Infinity;
    const deltaZ = dirZ !== 0 ? Math.abs(1 / dirZ) : Infinity;
    let nextX = dirX !== 0 ? (ix + (dirX > 0 ? 1 : 0) - cx) / dirX : Infinity;
    let nextZ = dirZ !== 0 ? (iz + (dirZ > 0 ? 1 : 0) - cz) / dirZ : Infinity;
    enter = NEAR;
    let face = SIDE_Z;
    let steps = 0;
    let foot = 0;
    waterLo = H;
    waterHi = 0;
    // The foliage the ray just came through, so leaves behind leaves read as the inside of a crown.
    leafLo = 0;
    leafHi = 0;

    while (ix >= 0 && iz >= 0 && ix < n && iz < n && enter < FAR && lo < hi) {
      exit = Math.min(nextX, nextZ);
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
      const count = runCount[ci]!;
      const wx = cx + dirX * enter;
      const wz = cz + dirZ * enter;
      let nextLeafLo = 0;
      let nextLeafHi = 0;
      let plantAt = -1;

      // Faces toward the camera first: they hide the tops behind them.
      sFace = face;
      sX = wx;
      sZ = wz;
      sDepth = enter;
      sScale = scaleIn;
      sFoot = foot;
      sInner = -1;
      for (let k = -1; k < count; k++) {
        let y0 = 0;
        let y1 = groundTop;
        sMat = groundMat[ci]!;
        if (k >= 0) {
          const ro = (ci * RUNS + k) * 3;
          y0 = runs[ro]!;
          y1 = runs[ro + 1]!;
          sMat = runs[ro + 2]!;
          if (sMat >= PLANT) {
            plantAt = ro;
            continue;
          }
          if (sMat >= FOLIAGE && sMat < LAVA) {
            if (nextLeafHi === 0) nextLeafLo = y0;
            nextLeafHi = y1;
          }
        }
        // Of the ground's side only what stands above the column in front is exposed.
        if (k < 0 && (y1 <= foot || (y0 = foot) < 0)) continue;
        sLip = k < 0 ? g : -1;
        fill(hz - (y1 - cy) * scaleIn, hz - (y0 - cy) * scaleIn, wall);
      }
      // A plant is two cut-outs crossing the cell corner to corner; the ray meets each once.
      if (plantAt >= 0 && scaleIn / 16 > 0.16) {
        const sumD = dirX + dirZ;
        const difD = dirX - dirZ;
        const tA = difD !== 0 ? (ix - cx - (iz - cz)) / difD : -1;
        const tB = sumD !== 0 ? (1 + ix + iz - cx - cz) / sumD : -1;
        sMat = runs[plantAt + 2]!;
        sLow = runs[plantAt]!;
        sHigh = runs[plantAt + 1]!;
        for (let pass = 0; pass < 2; pass++) {
          const t = pass === 0 ? Math.min(tA, tB) : Math.max(tA, tB);
          if (t <= enter || t >= exit) continue;
          sX = cx + dirX * t;
          sAcross = sX - ix;
          if (sAcross < 0 || sAcross > 1) continue;
          sZ = cz + dirZ * t;
          sDepth = t;
          sScale = focal / t;
          fill(hz - (sHigh - cy) * sScale, hz - (sLow - cy) * sScale, sprite);
        }
      }
      // Then tops seen from above and undersides seen from below.
      sCell = ci;
      sSlant = true;
      for (let k = -1; k < count; k++) {
        let y0 = 0;
        let y1 = groundTop;
        sMat = wet ? WATER : groundMat[ci]!;
        if (k >= 0) {
          const ro = (ci * RUNS + k) * 3;
          y0 = runs[ro]!;
          y1 = runs[ro + 1]!;
          sMat = runs[ro + 2]!;
          if (sMat >= PLANT || sMat === VINE) continue;
        }
        const above = cy > y1;
        if (!above && (k < 0 || cy >= y0)) continue;
        // The face's far edge and near edge on screen: a top lies below the horizon, an underside above it.
        sRise = above ? cy - y1 : cy - y0;
        sHigh = above ? y1 : y0;
        sFace = above ? TOP : UNDER;
        sInner = above ? 1 : 0.8;
        sOver = k < 0 && above ? decal[ci]! : 0;
        fill(hz + sRise * (above ? scaleOut : scaleIn), hz + sRise * (above ? scaleIn : scaleOut), level);
      }
      // Foliage is seen from inside too: the far faces of a leafy block close the gaps of its near ones.
      sOver = 0;
      sSlant = false;
      sInner = 0.55;
      sLip = -1;
      sFoot = 0;
      for (let k = nextLeafHi === 0 ? count : 0; k < count; k++) {
        const ro = (ci * RUNS + k) * 3;
        sMat = runs[ro + 2]!;
        if (sMat < FOLIAGE || sMat >= LAVA) continue;
        const y0 = runs[ro]!;
        const y1 = runs[ro + 1]!;
        // Its far side, then its top from below or its underside from above.
        sFace = nextX < nextZ ? SIDE_X : SIDE_Z;
        sX = cx + dirX * exit;
        sZ = cz + dirZ * exit;
        sDepth = exit;
        sScale = scaleOut;
        fill(hz - (y1 - cy) * scaleOut, hz - (y0 - cy) * scaleOut, wall);
        if (sMat === VINE || (cy >= y0 && cy <= y1)) continue;
        sRise = cy - (cy < y0 ? y1 : y0);
        sHigh = cy - sRise;
        sFace = UNDER;
        const a = hz + sRise * scaleIn;
        const b = hz + sRise * scaleOut;
        fill(a < b ? a : b, a < b ? b : a, level);
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
        left[row] = 0;
        if (share >= 0.5 || depths[row * W + x] === 0) depths[row * W + x] = depth;
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
      sky.into(skyTap, 0, row, true);
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
      if (!mirrored[row] && waterDepth[row] !== 0) sky.into(m, row * 3, 2 * hz - row - 1, false);
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
      else sky.into(skyTap, 0, hz - climb * focal, false);
      // Waves too fine to see still scatter the light: the rougher the water, the wider and fainter its glitter.
      const sunlit = shadows.lit(wx, surface + 0.05, wz);
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
    finishing: () => finishing(hdr, depths, view, sky, light),
  };
}
