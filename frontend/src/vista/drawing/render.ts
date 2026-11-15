import { bloom, quartered, soften, tone } from './finish';
import { Light, Tops, clamp01, hash3 } from './light';
import { Sky, type Ray, type View } from './sky';
import { BANDS, BELOW, FACES, GLOWS, RUNS, TERRACOTTA, WATER } from './blocks';
import { GRID, noise } from './land';
import { ASPECT, SHALLOW, quarter, type Quarter, type Strip } from './strips';
import { CLAY_PLAIN } from './paints';
import type { World } from './world';

/**
 * Draws a world to pixels on the CPU, cheaply enough for the slowest machine: every face of a block one flat
 * colour, lit by the few colours of the world's evening.
 *
 * The camera never rolls or pitches (the horizon is placed by shifting the image instead), so every block edge that
 * is vertical in the world is vertical on screen. A screen column therefore crosses the grid along one 2D line: walk
 * that line once, front to back, and each column of blocks it meets projects to a few vertical spans (the face
 * toward the camera, then the top or underside). Cost follows the number of pixels, not rays times steps. Water
 * mirrors the same walk into the pixels the first pass marked as water.
 *
 * A surface takes the sky's fill and, where it is turned to the light, the light's own colour, both thinned under
 * foliage as the game thins its sky light; lanterns and windows add the game's block light; then the game's own
 * shading of each face and of each inner corner is laid over the lot.
 */
export interface Renderer {
  /**
   * Draws screen columns `from` up to `to` and keeps their light. Columns are independent, so renderers of one
   * scene may share a picture between them in any strips that begin on a multiple of four. Returns what the strip
   * gives the whole picture's bloom.
   */
  draw(from: number, to: number): Quarter;
  /** With the whole picture's bloom source, turns each strip drawn into display pixels and lets it go. */
  finish(soft: Float32Array): Strip[];
}

const NEAR = 0.2;
const FAR = 260;
const TOP = 0;
const SIDE_X = 1;
const SIDE_Z = 2;
const UNDER = 3;
/** How rough a block's face is, as the width of the lobe it gives a light back in, squared. */
const ROUGH = 0.9 ** 4;
/** How much of a rough face is neither hidden from nor shaded for a direction it meets at `cos`. */
const masked = (cos: number): number => 2 / (1 + Math.sqrt(1 + (ROUGH * (1 - cos * cos)) / (cos * cos)));
/** The game's own shading of a block's faces, squared: top, the two pairs of sides, underside. */
const DIFFUSE = [1, 0.49, 0.7225, 0.0625];
/** How many columns a side the tiles are that a line of light skips over whole. */
const TILE = 8;
/** How far off, in blocks, a wall's two kinds of face begin to be lit alike, and are wholly. */
const FLAT_FROM = 45;
const FLAT_TO = 110;

/**
 * The stretch of sky the last light comes from, as turns to either side of it and lifts above and below, in
 * radians; how much of it is the sky's glow, which casts no shadow; and how much of the sky's light still
 * reaches a place under foliage.
 */
const SUN: readonly (readonly [number, number])[] = [
  [0, 0.05],
  [0.07, -0.03],
  [-0.07, -0.03],
];
const GLOWING = 0.22;
const SHELTERED = 0.4;

/** How much of an open water's swell a sheltered one keeps. */
const CALM = 0.2;

/**
 * A shallow picture: how much of a whole picture's height it shows at the least, how far down it the building's
 * foot stands, how much of its height the building may take, and the widest lens it is drawn through, as the
 * slope of its edge from straight ahead.
 */
const BAND = 0.72;
const STANDS = 0.62;
const ROOM = 0.58;
const WIDEST = 1.1;

/**
 * A renderer of `world` at `width` by `height`. A picture much shallower than a whole one is no crop of it: it
 * shows the building whole, at the size a whole picture of the same height would, with as much of the land to
 * either side as its width takes in.
 */
export function createRenderer(world: World, width: number, height: number): Renderer {
  const n = GRID;
  const { ground, groundMat, runs, runCount, skyLight, lamps, colors, camera } = world;
  const W = width;
  const H = height;
  const lens = Math.tan(camera.fov / 2);
  const shallow = W > H * ASPECT * SHALLOW;
  // A shallow picture is as much of a whole one, top to bottom, as holds the building with room to spare.
  const shown = Math.max(BAND, (camera.foot - camera.top) / ROOM);
  const focal = shallow ? Math.max(((H / shown) * ASPECT) / 2 / lens, W / 2 / WIDEST) : W / 2 / lens;
  const hz = shallow ? STANDS * H + ((camera.horizon - camera.foot) * focal * 2 * lens) / ASPECT : H * camera.horizon;
  const cx = camera.x;
  const cy = camera.y;
  const cz = camera.z;
  const fwdX = Math.sin(camera.yaw);
  const fwdZ = Math.cos(camera.yaw);
  const hasWater = world.waterLevel > 0;
  const surface = world.waterLevel - 0.14;

  const light = new Light(world);
  const { key: keyColor, fill: fillColor, glows, water: waterColor, x: lightX, y: lightY, z: lightZ } = light;
  const view: View = { width: W, height: H, focal, horizon: hz, x: cx, y: cy, z: cz, yaw: camera.yaw };
  const ray: Ray = { dirX: 0, dirZ: 0, span: 1 };
  const sky = new Sky(light, view, ray);
  const tops = new Tops(world);
  const { solid: solidTop, leaf: leafTop, rim } = tops;
  const { back, backColor, lamp: lampColor } = light;

  // The strip being drawn: its light, and depth per pixel, so edges between near and far can be smoothed. It
  // starts at column `origin`, is `wide` columns across, and holds a column of margin on each side for that.
  let hdr = new Float32Array(0);
  let depths = new Float32Array(0);
  let origin = 0;
  let wide = 0;
  let stride = 0;
  const held: { origin: number; wide: number; from: number; to: number; hdr: Float32Array }[] = [];
  const mirror = hasWater ? new Float32Array(H * 3) : null;
  // The light of what glows on the water's bed, as it reaches the surface: dimmer the deeper it lies, and spread
  // a few blocks about, as water spreads it.
  let bedGlow: Float32Array | null = null;
  if (hasWater) {
    for (let z = 3; z < n - 3; z++) {
      for (let x = 3; x < n - 3; x++) {
        const i = z * n + x;
        const mat = groundMat[i]!;
        if (GLOWS[mat] === 0 || ground[i]! >= world.waterLevel) continue;
        bedGlow ??= new Float32Array(n * n * 3);
        const dim = Math.exp((ground[i]! - surface) * 0.22) / 2.2;
        for (let dz = -2; dz <= 2; dz++) {
          for (let dx = -2; dx <= 2; dx++) {
            const share = Math.max(0, 1 - Math.hypot(dx, dz) / 2.2) * dim;
            const o = (i + dz * n + dx) * 3;
            bedGlow[o] += glows[mat * 3]! * share;
            bedGlow[o + 1] += glows[mat * 3 + 1]! * share;
            bedGlow[o + 2] += glows[mat * 3 + 2]! * share;
          }
        }
      }
    }
  }
  const waterDepth = new Float32Array(H);
  const waterCell = new Int32Array(H);
  // The water's swell: trains of waves of several lengths crossing at angles round the wind.
  // Each is (x step, z step, height, length), longest first.
  const WAVES = 6;
  const waves = new Float32Array(WAVES * 4);
  {
    const wind = hash3(world.seed, 3, 5) * 6.283;
    const lengths = [7.5, 4.6, 3.1, 2.1, 1.3, 0.8];
    const slopes = [0.012, 0.014, 0.014, 0.012, 0.008, 0.005].map((slope) => slope * CALM);
    for (let i = 0; i < WAVES; i++) {
      const turn = wind + (hash3(world.seed, i, 9) - 0.5) * 2.6;
      const k = 6.283 / lengths[i]!;
      waves.set([Math.sin(turn) * k, Math.cos(turn) * k, slopes[i]! / k, lengths[i]!], i * 4);
    }
  }
  let slopeX = 0;
  let slopeZ = 0;
  /**
   * The slope of the water at a point, into `slopeX` and `slopeZ`. `grain` is how much water one pixel spans there:
   * waves too short to show at that size are left out.
   */
  const swell = (x: number, z: number, grain: number): void => {
    // The trains bend a little, or they would lie as straight as corduroy.
    const qx = x + Math.sin(z * 0.23 + 1.7) * 0.9;
    const qz = z + Math.sin(x * 0.19 + 0.4) * 0.9;
    // The water lies calmer in some stretches than in others.
    const gust = 1 + 0.45 * noise(x / 11, z / 11, world.seed + 91);
    slopeX = 0;
    slopeZ = 0;
    for (let o = 0; o < WAVES * 4; o += 4) {
      const kx = waves[o]!;
      const kz = waves[o + 1]!;
      const shown = waves[o + 3]! / (grain * 5);
      const steep = waves[o + 2]! * gust * Math.cos(kx * qx + kz * qz + o * 1.7);
      if (shown < 1) {
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
  const mirrored = new Uint8Array(H);
  const pathCell = new Int32Array(4 * n);
  const pathEnter = new Float32Array(4 * n);
  const pathExit = new Float32Array(4 * n);
  const pathFace = new Uint8Array(4 * n);

  // Per-column state the shading closures read.
  let dirX = 0;
  let dirZ = 0;
  let span = 1;
  let lo = 0;
  let hi = 0;

  const skyTap = new Float32Array(3);
  // How much of a far thing the air hides, tabled by distance: each line of trees stands paler than the one before.
  const [fogR, fogG, fogB] = light.haze.color;
  const hazes = new Float32Array(FAR + 2);
  for (let depth = 0; depth < hazes.length; depth++) {
    const t = clamp01((depth - light.haze.from) / (light.haze.to - light.haze.from));
    hazes[depth] = light.haze.most * t * t * (3 - 2 * t);
  }
  /** How much of something at a distance the air hides. */
  const haze = (depth: number): number => hazes[depth > FAR ? FAR : depth | 0]!;

  // The highest thing in each column that stands in a light's way, and the highest of them all.
  const peak = new Uint8Array(n * n);
  let ceiling = 0;
  for (let i = 0; i < n * n; i++) {
    peak[i] = Math.max(solidTop[i]!, leafTop[i]!);
    if (peak[i]! > ceiling) ceiling = peak[i]!;
  }
  // The highest thing in each tile of columns: a rising line already over a tile's highest crosses it at a step.
  const tiles = n / TILE;
  const tilePeak = new Uint8Array(tiles * tiles);
  for (let i = 0; i < n * n; i++) {
    const tile = Math.floor(i / n / TILE) * tiles + Math.floor((i % n) / TILE);
    if (peak[i]! > tilePeak[tile]!) tilePeak[tile] = peak[i]!;
  }
  /**
   * Whether anything solid stands between a point and the one `far` times (dx, dy, dz) from it, leaving the
   * column `but` out: the line is walked column by column, exactly, so a shadow's edge is the block's own. What
   * gives light stands in no light's way.
   */
  const blocked = (
    px: number,
    py: number,
    pz: number,
    dx: number,
    dy: number,
    dz: number,
    far: number,
    but: number,
  ): boolean => {
    let bx = Math.floor(px);
    let bz = Math.floor(pz);
    const stepX = dx > 0 ? 1 : -1;
    const stepZ = dz > 0 ? 1 : -1;
    const perX = dx === 0 ? Infinity : Math.abs(1 / dx);
    const perZ = dz === 0 ? Infinity : Math.abs(1 / dz);
    let nextX = dx === 0 ? Infinity : (dx > 0 ? bx + 1 - px : px - bx) * perX;
    let nextZ = dz === 0 ? Infinity : (dz > 0 ? bz + 1 - pz : pz - bz) * perZ;
    let from = py;
    for (;;) {
      if (bx < 0 || bz < 0 || bx >= n || bz >= n) return false;
      if (dy >= 0) {
        const tx = Math.floor(bx / TILE);
        const tz = Math.floor(bz / TILE);
        if (from >= tilePeak[tz * tiles + tx]!) {
          // Nothing in this tile reaches the line: on to where it leaves the tile.
          const outX = dx > 0 ? ((tx + 1) * TILE - px) / dx : dx < 0 ? (tx * TILE - px) / dx : Infinity;
          const outZ = dz > 0 ? ((tz + 1) * TILE - pz) / dz : dz < 0 ? (tz * TILE - pz) / dz : Infinity;
          const out = Math.min(outX, outZ) + 1e-4;
          if (out >= far) return false;
          bx = Math.floor(px + dx * out);
          bz = Math.floor(pz + dz * out);
          nextX = dx === 0 ? Infinity : ((dx > 0 ? bx + 1 : bx) - px) / dx;
          nextZ = dz === 0 ? Infinity : ((dz > 0 ? bz + 1 : bz) - pz) / dz;
          from = py + dy * out;
          continue;
        }
      }
      const leave = Math.min(nextX, nextZ, far);
      const to = py + dy * leave;
      const low = from < to ? from : to;
      const i = bz * n + bx;
      if (low < peak[i]! && i !== but) {
        if (low < ground[i]!) return true;
        const high = from < to ? to : from;
        for (let k = 0; k < runCount[i]!; k++) {
          const ro = (i * RUNS + k) * 3;
          if (high > runs[ro]! && low < runs[ro + 1]! && GLOWS[runs[ro + 2]!]! === 0) return true;
        }
      } else if (dy >= 0 && low >= ceiling) return false;
      if (leave >= far) return false;
      if (nextX < nextZ) {
        bx += stepX;
        nextX += perX;
      } else {
        bz += stepZ;
        nextZ += perZ;
      }
      from = to;
    }
  };
  // The last light is no point: it comes from a stretch of the sky some degrees across, so a shadow's edge is
  // soft, and softer the further it falls from what casts it. A few directions across that stretch stand for it.
  const suns: number[] = [];
  {
    const bearing = Math.atan2(lightX, lightZ);
    const height = Math.asin(lightY);
    for (const [turn, lift] of SUN) {
      const up = height + lift;
      suns.push(Math.sin(bearing + turn) * Math.cos(up), Math.sin(up), Math.cos(bearing + turn) * Math.cos(up));
    }
  }
  /** How much of the last light reaches a point, from none to all. */
  const reaches = (px: number, py: number, pz: number): number => {
    let open = 0;
    for (let at = 0; at < suns.length; at += 3)
      if (!blocked(px, py, pz, suns[at]!, suns[at + 1]!, suns[at + 2]!, (ceiling - py) / suns[at + 1]!, -1)) open++;
    return open / (suns.length / 3);
  };
  /** How far a lantern's light carries, in blocks. */
  const CARRY = 22;
  const lampTap = new Float32Array(3);
  /**
   * What the lanterns give a point on a face turned along (nx, ny, nz), into `lampTap`: each by how squarely the
   * face meets it and how far off it is, where nothing stands between.
   */
  const lanterns = (px: number, py: number, pz: number, nx: number, ny: number, nz: number): void => {
    lampTap.fill(0);
    for (let at = 0; at < lamps.length; at += 5) {
      const dx = lamps[at]! - px;
      const dy = lamps[at + 1]! - py;
      const dz = lamps[at + 2]! - pz;
      const far2 = dx * dx + dy * dy + dz * dz;
      if (far2 > CARRY * CARRY) continue;
      const far = Math.sqrt(far2);
      const facing = (dx * nx + dy * ny + dz * nz) / far;
      if (facing <= 0) continue;
      if (blocked(px, py, pz, dx, dy, dz, 1, lamps[at + 4]!)) continue;
      // It falls off with the square of the distance, and is eased out to nothing where it stops carrying.
      const fade = 1 - far2 / (CARRY * CARRY);
      const given = (lamps[at + 3]! * facing * fade * fade) / Math.max(1, far2);
      lampTap[0] += lampColor[0] * given;
      lampTap[1] += lampColor[1] * given;
      lampTap[2] += lampColor[2] * given;
    }
  };

  /** How dark an inner corner is where two sides and the block diagonally between them meet, as the game has it. */
  const meet = (first: number, second: number, blocked: boolean): number =>
    (1 + first + second + (first + second < 1 || blocked ? 0.2 : 1)) / 4;
  /**
   * Lights one surface point, adding `share` of it at `o` in `out`. `lip` is the surface height when the point is on
   * the side of a ground column, else -1. On a top, `foot` is -1 where neighbours cast no corner shade.
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
    lip: number,
    foot: number,
    share: number,
  ): void => {
    const mist = haze(depth);
    const shine = GLOWS[mat]!;
    if (shine > 0) {
      // What glows is lit by nothing but itself, and shows through the air further than what does not.
      const veil = mist * 0.5;
      out[o] += (glows[mat * 3]! + (fogR - glows[mat * 3]!) * veil) * share;
      out[o + 1] += (glows[mat * 3 + 1]! + (fogG - glows[mat * 3 + 1]!) * veil) * share;
      out[o + 2] += (glows[mat * 3 + 2]! + (fogB - glows[mat * 3 + 2]!) * veil) * share;
      return;
    }
    // The block the face belongs to: a side lies exactly on the line between two.
    const bx = face === SIDE_X ? Math.round(wx) - (dirX > 0 ? 0 : 1) : Math.floor(wx);
    const bz = face === SIDE_Z ? Math.round(wz) - (dirZ > 0 ? 0 : 1) : Math.floor(wz);
    const by = Math.ceil(wy) - 1;
    let tex = FACES[mat * 3 + (face === TOP ? 0 : face === UNDER ? 2 : 1)]!;
    // Below the surface block a column is soil, then rock.
    if (lip - wy > 1) tex = BELOW[mat * 2 + (lip - wy > 4 ? 1 : 0)]!;
    if (mat === TERRACOTTA || tex === CLAY_PLAIN) tex = BANDS[by % BANDS.length]!;
    // No block is quite its neighbour's colour.
    const tone = 0.86 + hash3(bx, by, bz) * 0.28;
    let r = colors[tex * 3]! * tone;
    let g = colors[tex * 3 + 1]! * tone;
    let b = colors[tex * 3 + 2]! * tone;

    // Light is read in the air in front of the face, as the game reads it.
    const nx = face === SIDE_X ? (dirX > 0 ? -1 : 1) : 0;
    const nz = face === SIDE_Z ? (dirZ > 0 ? -1 : 1) : 0;
    const ax = wx + nx * 0.5;
    const az = wz + nz * 0.5;
    const fx = Math.floor(ax);
    const fz = Math.floor(az);
    let vault = 1;
    let corner = 1;
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
        vault =
          (skyLight[a]! * w00 + skyLight[a + 1]! * w10 + skyLight[a + n]! * w01 + skyLight[a + n + 1]! * w11) / 15;
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
          corner =
            (meet(west, north, solidTop[fi - n - 1]! > high) * (1 - px) +
              meet(east, north, solidTop[fi - n + 1]! > high) * px) *
              (1 - pz) +
            (meet(west, south, solidTop[fi + n - 1]! > high) * (1 - px) +
              meet(east, south, solidTop[fi + n + 1]! > high) * px) *
              pz;
        }
      } else if (face !== UNDER) {
        // On a side: the block under the air in front, and the ones to either hand of it.
        const hand = face === SIDE_X ? n : 1;
        const under = solidTop[fi]! > by - 0.5 ? 0.2 : 1;
        const lowLeft = solidTop[fi - hand]!;
        const lowRight = solidTop[fi + hand]!;
        if (under < 1 || lowLeft > by - 0.5 || lowRight > by - 0.5) {
          const leftSide = lowLeft > by + 0.5 ? 0.2 : 1;
          const rightSide = lowRight > by + 0.5 ? 0.2 : 1;
          const across = face === SIDE_X ? wz - Math.floor(wz) : wx - Math.floor(wx);
          const rise = wy - by;
          corner =
            (meet(under, leftSide, lowLeft > by - 0.5) * (1 - across) +
              meet(under, rightSide, lowRight > by - 0.5) * across) *
              (1 - rise) +
            (meet(1, leftSide, lowLeft > by + 1.5) * (1 - across) + meet(1, rightSide, lowRight > by + 1.5) * across) *
              rise;
        }
      }
    }

    // How squarely the face meets each light. The sky's own fill falls more on what is turned up to it.
    const ny = face === TOP ? 1 : face === UNDER ? -1 : 0;
    let facing = Math.max(0, nx * lightX + ny * lightY + nz * lightZ);
    let behind = Math.max(0, nx * back[0] + ny * back[1] + nz * back[2]);
    let upright = DIFFUSE[face]!;
    if (face === SIDE_X || face === SIDE_Z) {
      // A wall that runs aslant the blocks is a stair of faces turned two ways. Near by each is lit as it is
      // turned; far off, where the stair is too fine to read, they would show as stripes down the wall, so both
      // take the light as the wall between them would, and its creases are let go.
      const far = clamp01((depth - FLAT_FROM) / (FLAT_TO - FLAT_FROM));
      const flat = far * far * (3 - 2 * far);
      const sx = dirX > 0 ? -1 : 1;
      const sz = dirZ > 0 ? -1 : 1;
      facing += ((Math.max(0, sx * lightX) + Math.max(0, sz * lightZ)) / 2 - facing) * flat;
      behind += ((Math.max(0, sx * back[0]) + Math.max(0, sz * back[2])) / 2 - behind) * flat;
      upright += ((DIFFUSE[SIDE_X]! + DIFFUSE[SIDE_Z]!) / 2 - upright) * flat;
      corner += (1 - corner) * flat;
    }
    const reached = facing > 0 ? reaches(wx + nx * 0.02, wy + ny * 0.02, wz + nz * 0.02) : 0;
    // Under foliage some of the sky's light still comes in from the sides.
    const open = SHELTERED + (1 - SHELTERED) * vault * vault;
    // Part of the last light is the glow of the sky round it, which nothing shadows; the rest is cast.
    const sunlit = facing * (reached * (1 - GLOWING) + GLOWING * corner * open);
    const around = upright * corner * open;
    lanterns(wx + nx * 0.06, wy + ny * 0.06, wz + nz * 0.06, nx, ny, nz);
    // A bright block is held back a little, so pale ones do not burn out.
    const hold = 1 / (1 + Math.max(r, g, b) * 0.25);
    r *=
      (fillColor[0] * around + keyColor[0] * sunlit + backColor[0] * behind * corner * open + lampTap[0]! * corner) *
      hold;
    g *=
      (fillColor[1] * around + keyColor[1] * sunlit + backColor[1] * behind * corner * open + lampTap[1]! * corner) *
      hold;
    b *=
      (fillColor[2] * around + keyColor[2] * sunlit + backColor[2] * behind * corner * open + lampTap[2]! * corner) *
      hold;
    if (reached > 0) {
      // A rough face still gives the light back at a glance: with the light low ahead, its colour lies along
      // every top and edge it reaches, whatever the block's own.
      const vy = (wy - cy) / depth;
      const length = Math.sqrt(span + vy * vy);
      const seen = Math.max(0.02, -(dirX * nx + vy * ny + dirZ * nz) / length);
      let hx = lightX - dirX / length;
      let hy = lightY - vy / length;
      let hz = lightZ - dirZ / length;
      const half = Math.hypot(hx, hy, hz) || 1;
      hx /= half;
      hy /= half;
      hz /= half;
      const across = nx * hx + ny * hy + nz * hz;
      const lobe = across * across * (ROUGH - 1) + 1;
      const glance = (1 - Math.max(0, lightX * hx + lightY * hy + lightZ * hz)) ** 5;
      const given =
        ((ROUGH / (lobe * lobe)) * (0.04 + 0.96 * glance) * masked(facing) * masked(seen) * reached) / (4 * seen);
      r += keyColor[0] * given;
      g += keyColor[1] * given;
      b += keyColor[2] * given;
    }
    out[o] += (r + (fogR - r) * mist) * share;
    out[o + 1] += (g + (fogG - g) * mist) * share;
    out[o + 2] += (b + (fogB - b) * mist) * share;
  };

  // The column being drawn, and the span of it being filled: what the span is made of and where it lies.
  let column = 0;
  let base = 0;
  let enter = NEAR;
  let exit = NEAR;
  let waterLo = H;
  let waterHi = 0;
  let sMat = 0;
  let sFace = 0;
  let sX = 0;
  let sZ = 0;
  let sDepth = 0;
  let sScale = 0;
  let sLip = -1;
  let sFoot = 0;
  let sHigh = 0;
  let sRise = 0;
  let sCell = 0;

  /**
   * Fills the rows a span covers from screen height `top` to `bottom`. Each row takes all of what is left of its
   * pixel inside the span, and only the covered fraction where the span begins and ends; `point` shades the
   * middle of what the row covers and returns its depth.
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
      left[row] = rest - share < 0.004 ? 0 : rest - share;
      if (share >= 0.5 || depths[row * wide + column] === 0) depths[row * wide + column] = depth;
    }
  };
  /** A point on an upright face of a block. */
  const wall = (at: number, row: number, share: number): number => {
    shade(hdr, row * stride + base, sMat, sFace, sX, cy - (at - hz) / sScale, sZ, sDepth, sLip, sFoot, share);
    return sDepth;
  };
  /** A point on a top seen from above or an underside seen from below; open water is only noted, for the mirror. */
  const level = (at: number, row: number, share: number): number => {
    const depth = Math.min(exit, Math.max(enter, (sRise * focal) / (at - hz)));
    if (sMat === WATER) {
      waterShare[row] += share;
      waterDepth[row] = depth;
      waterCell[row] = sCell;
      if (row < waterLo) waterLo = row;
      if (row >= waterHi) waterHi = row + 1;
    } else
      shade(hdr, row * stride + base, sMat, sFace, cx + dirX * depth, sHigh, cz + dirZ * depth, depth, -1, 0, share);
    return depth;
  };

  const drawColumn = (x: number): void => {
    const u = (x + 0.5 - W / 2) / focal;
    dirX = fwdX + fwdZ * u;
    dirZ = fwdZ - fwdX * u;
    span = 1 + u * u;
    ray.dirX = dirX;
    ray.dirZ = dirZ;
    ray.span = span;
    left.fill(1);
    lo = 0;
    hi = H;
    column = x - origin;
    base = column * 3;

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
      const groundTop = wet ? surface : g;
      const scaleIn = focal / enter;
      const scaleOut = focal / exit;
      const count = runCount[ci]!;

      // Faces toward the camera first: they hide the tops behind them.
      sFace = face;
      sX = cx + dirX * enter;
      sZ = cz + dirZ * enter;
      sDepth = enter;
      sScale = scaleIn;
      sFoot = foot;
      for (let k = -1; k < count; k++) {
        let y0 = 0;
        let y1 = groundTop;
        sMat = groundMat[ci]!;
        if (k >= 0) {
          const ro = (ci * RUNS + k) * 3;
          y0 = runs[ro]!;
          y1 = runs[ro + 1]!;
          sMat = runs[ro + 2]!;
        }
        // Of the ground's side only what stands above the column in front is exposed.
        if (k < 0 && (y1 <= foot || (y0 = foot) < 0)) continue;
        sLip = k < 0 ? g : -1;
        fill(hz - (y1 - cy) * scaleIn, hz - (y0 - cy) * scaleIn, wall);
      }
      // Then tops seen from above and undersides seen from below.
      sCell = ci;
      for (let k = -1; k < count; k++) {
        let y0 = 0;
        let y1 = groundTop;
        sMat = wet ? WATER : groundMat[ci]!;
        if (k >= 0) {
          const ro = (ci * RUNS + k) * 3;
          y0 = runs[ro]!;
          y1 = runs[ro + 1]!;
          sMat = runs[ro + 2]!;
        }
        const above = cy > y1;
        if (!above && (k < 0 || cy >= y0)) continue;
        // The face's far edge and near edge on screen: a top lies below the horizon, an underside above it.
        sRise = above ? cy - y1 : cy - y0;
        sHigh = above ? y1 : y0;
        sFace = above ? TOP : UNDER;
        fill(hz + sRise * (above ? scaleOut : scaleIn), hz + sRise * (above ? scaleIn : scaleOut), level);
      }
      while (lo < hi && left[lo] === 0) lo++;
      while (hi > lo && left[hi - 1] === 0) hi--;

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
        if (share >= 0.5 || depths[row * wide + column] === 0) depths[row * wide + column] = depth;
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
      sky.into(skyTap, 0, row);
      const o = row * stride + base;
      hdr[o] += skyTap[0]! * share;
      hdr[o + 1] += skyTap[1]! * share;
      hdr[o + 2] += skyTap[2]! * share;
      if (share >= 0.5 || depths[row * wide + column] === 0) depths[row * wide + column] = FAR * 4;
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
        }
        if (y1 <= surface) continue;
        const from = Math.max(waterLo, Math.ceil(hz + (flip + y0) * scaleIn - 0.5));
        const to = Math.min(waterHi, Math.ceil(hz + (flip + y1) * scaleIn - 0.5));
        for (let row = from; row < to; row++) {
          if (mirrored[row] || waterDepth[row] === 0) continue;
          const wy = (row + 0.5 - hz) / scaleIn - flip;
          shade(m, row * 3, mat, pathFace[s]!, wx, wy, wz, pathEnter[s]!, k < 0 ? g : -1, surface, 1);
          mirrored[row] = 1;
          todo--;
        }
        if (k >= 0 && y0 > surface) {
          const under = Math.max(waterLo, Math.ceil(hz + (flip + y0) * scaleOut - 0.5));
          const until = Math.min(waterHi, Math.ceil(hz + (flip + y0) * scaleIn - 0.5));
          for (let row = under; row < until; row++) {
            if (mirrored[row] || waterDepth[row] === 0) continue;
            const depth = ((flip + y0) * focal) / (row + 0.5 - hz);
            shade(m, row * 3, mat, UNDER, cx + dirX * depth, y0, cz + dirZ * depth, depth, -1, 0, 1);
            mirrored[row] = 1;
            todo--;
          }
        }
      }
    }
    for (let row = waterLo; row < waterHi; row++) {
      if (!mirrored[row] && waterDepth[row] !== 0) sky.into(m, row * 3, 2 * hz - row - 1);
    }
    // Water: a slow swell tilts the surface, which bends the reflection; over its own dark colour lies the mirror,
    // strongest at a glancing angle.
    const eye = cy - surface;
    for (let row = waterLo; row < waterHi; row++) {
      const depth = waterDepth[row]!;
      if (depth === 0) continue;
      swell(cx + dirX * depth, cz + dirZ * depth, (depth * depth) / (focal * eye));
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
      // A ray bent off the water's own rows is taken unbent: the bank it would meet has no mirror image.
      const tap = from >= waterLo && from < waterHi && waterDepth[from] !== 0 ? from : row;
      skyTap.set(m.subarray(tap * 3, tap * 3 + 3));
      const grazing = 1 - cos;
      const fresnel = 0.02 + 0.98 * grazing * grazing * grazing * grazing * grazing;
      // Still water far off gives the sky back more than the air between hides it: open sea meets the sky's
      // own light at the horizon.
      const mist = haze(depth) * 0.3;
      // What glows on the bed shows up through the water, less the flatter the water is seen.
      const gx = cx + dirX * depth - 0.5;
      const gz = cz + dirZ * depth - 0.5;
      const bx = Math.floor(gx);
      const bz = Math.floor(gz);
      let bed = -1;
      let b00 = 0;
      let b10 = 0;
      let b01 = 0;
      let b11 = 0;
      if (bedGlow && bx >= 0 && bz >= 0 && bx < n - 1 && bz < n - 1) {
        bed = (bz * n + bx) * 3;
        const tx = gx - bx;
        const tz = gz - bz;
        b00 = (1 - tx) * (1 - tz) * (1 - fresnel);
        b10 = tx * (1 - tz) * (1 - fresnel);
        b01 = (1 - tx) * tz * (1 - fresnel);
        b11 = tx * tz * (1 - fresnel);
      }
      for (let c = 0; c < 3; c++) {
        let body = waterColor[c]! * (fillColor[c]! + keyColor[c]! * lightY);
        if (bed >= 0)
          body +=
            bedGlow![bed + c]! * b00 +
            bedGlow![bed + 3 + c]! * b10 +
            bedGlow![bed + n * 3 + c]! * b01 +
            bedGlow![bed + n * 3 + 3 + c]! * b11;
        const shown = body + (skyTap[c]! - body) * fresnel;
        hdr[o + c] += (shown + ((c === 0 ? fogR : c === 1 ? fogG : fogB) - shown) * mist) * waterShare[row]!;
      }
    }
    waterDepth.fill(0, waterLo, waterHi);
    waterShare.fill(0, waterLo, waterHi);
  };

  const qw = quarter(W);
  const qh = quarter(H);
  return {
    draw(from, to) {
      origin = Math.max(0, from - 1);
      wide = Math.min(W, to + 1) - origin;
      stride = wide * 3;
      hdr = new Float32Array(wide * H * 3);
      depths = new Float32Array(wide * H);
      for (let x = origin; x < origin + wide; x++) drawColumn(x);
      const strip = { origin, wide, from, to, hdr };
      soften(strip, depths, W, H);
      held.push(strip);
      return { from, to, soft: quartered(strip, H) };
    },
    finish(soft) {
      const glow = bloom(soft, qw, qh);
      return held.splice(0).map((strip) => ({ from: strip.from, to: strip.to, pixels: tone(strip, view, glow) }));
    },
  };
}
