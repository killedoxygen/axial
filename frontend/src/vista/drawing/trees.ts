/**
 * The game's trees, each set down as runs of blocks in the columns it covers.
 */
import * as B from './blocks';
import { GRID, hash } from './land';

/** The world being planted, as far as a tree needs to know. */
export interface Plot {
  seed: number;
  ground: Uint8Array;
  /** Adds a run of `mat` from `y0` up to `y1` to a column, unless something is already there. */
  put(x: number, z: number, y0: number, y1: number, mat: number): void;
}

export type Tree = 'oak' | 'tall oak' | 'swamp oak' | 'birch' | 'spruce' | 'dark oak' | 'acacia' | 'cherry' | 'cactus';

/** What each kind of tree is made of. */
export const WOOD: Record<Tree, number[]> = {
  oak: [B.OAK_LOG, B.OAK_LEAVES],
  'tall oak': [B.OAK_LOG, B.OAK_LEAVES],
  'swamp oak': [B.OAK_LOG, B.OAK_LEAVES],
  birch: [B.BIRCH_LOG, B.BIRCH_LEAVES],
  spruce: [B.SPRUCE_LOG, B.SPRUCE_LEAVES],
  'dark oak': [B.DARK_OAK_LOG, B.DARK_OAK_LEAVES],
  acacia: [B.ACACIA_LOG, B.ACACIA_LEAVES],
  cherry: [B.CHERRY_LOG, B.CHERRY_LEAVES],
  cactus: [B.CACTUS],
};

/** The game's small tree: two wide layers of foliage, then two narrow ones, the top a plus. */
function oak(
  { ground, put, seed }: Plot,
  x: number,
  z: number,
  height: number,
  leaf: number,
  log: number,
  spread = 2,
): void {
  const g = ground[z * GRID + x]!;
  const crown = g + height;
  put(x, z, g, crown, log);
  put(x, z, crown, crown + 2, leaf);
  for (let dz = -spread; dz <= spread; dz++) {
    for (let dx = -spread; dx <= spread; dx++) {
      const ax = Math.abs(dx);
      const az = Math.abs(dz);
      if (ax + az === 0) continue;
      const keep = hash(x + dx, z + dz, seed + 31) > 0.45;
      const rim = ax === spread || az === spread;
      if (rim && ax === az && !keep) continue;
      const inner = ax < 2 && az < 2;
      put(x + dx, z + dz, crown - 2, crown + (!inner ? 0 : ax + az === 1 ? 2 : keep ? 1 : 0), leaf);
    }
  }
}

/** A tall trunk carrying several rounded clumps of foliage. */
function tallOak({ ground, put, seed }: Plot, x: number, z: number, height: number): void {
  const g = ground[z * GRID + x]!;
  const crown = g + height;
  put(x, z, g, crown, B.OAK_LOG);
  put(x, z, crown, crown + 3, B.OAK_LEAVES);
  for (let clump = 0; clump < 4; clump++) {
    const ox = clump === 0 ? 0 : Math.round((hash(x + clump, z, seed + 61) - 0.5) * 6);
    const oz = clump === 0 ? 0 : Math.round((hash(x, z + clump, seed + 63) - 0.5) * 6);
    const oy = crown - (clump === 0 ? 0 : 1 + clump);
    const radius = clump === 0 ? 2.7 : 2.2;
    for (let dz = -3; dz <= 3; dz++) {
      for (let dx = -3; dx <= 3; dx++) {
        const reach = radius * radius - dx * dx - dz * dz;
        if (reach < 0 || (ox + dx === 0 && oz + dz === 0)) continue;
        const half = Math.floor(Math.sqrt(reach) + 0.4);
        put(x + ox + dx, z + oz + dz, oy - half, oy + half + 1, B.OAK_LEAVES);
      }
    }
  }
}

function spruce({ ground, put }: Plot, x: number, z: number, height: number, leaf: number): void {
  const g = ground[z * GRID + x]!;
  // From the tip down, the foliage steps out and in.
  const radii = [0, 1, 2, 1, 2, 3, 2, 3];
  const layers = Math.min(height - 1, radii.length);
  put(x, z, g, g + height, B.SPRUCE_LOG);
  put(x, z, g + height, g + height + 1, leaf);
  for (let dz = -3; dz <= 3; dz++) {
    for (let dx = -3; dx <= 3; dx++) {
      const d = Math.max(Math.abs(dx), Math.abs(dz));
      if (d === 0) continue;
      const corner = Math.abs(dx) === d && Math.abs(dz) === d;
      let from = -1;
      for (let layer = layers - 1; layer >= -1; layer--) {
        const y = g + height - layer;
        const leafy = layer >= 0 && radii[layer]! >= d && !(corner && radii[layer] === d);
        if (leafy && from < 0) from = y;
        if (!leafy && from >= 0) {
          put(x + dx, z + dz, from, y, leaf);
          from = -1;
        }
      }
    }
  }
}

/** Four trunks side by side under one broad, deep crown. */
function darkOak({ ground, put, seed }: Plot, x: number, z: number, height: number): void {
  const crown = ground[z * GRID + x]! + height;
  for (let dz = -3; dz <= 4; dz++) {
    for (let dx = -3; dx <= 4; dx++) {
      const d = Math.hypot(dx - 0.5, dz - 0.5);
      if (d < 1) {
        put(x + dx, z + dz, ground[(z + dz) * GRID + x + dx]!, crown, B.DARK_OAK_LOG);
        put(x + dx, z + dz, crown, crown + 2, B.DARK_OAK_LEAVES);
      } else if (d < 4.1 && (d < 3.4 || hash(x + dx, z + dz, seed + 31) > 0.4))
        put(x + dx, z + dz, crown - 2, crown + (d < 2.2 ? 2 : d < 3.3 ? 1 : 0), B.DARK_OAK_LEAVES);
    }
  }
}

/** A trunk that leans over, then a wide flat crown two layers deep. */
function acacia({ ground, put, seed }: Plot, x: number, z: number, height: number): void {
  const g = ground[z * GRID + x]!;
  const turn = Math.floor(hash(x, z, seed + 65) * 4);
  const lx = turn === 0 ? 1 : turn === 1 ? -1 : 0;
  const lz = turn === 2 ? 1 : turn === 3 ? -1 : 0;
  put(x, z, g, g + height, B.ACACIA_LOG);
  put(x + lx, z + lz, g + height, g + height + 1, B.ACACIA_LOG);
  const cx = x + lx * 2;
  const cz = z + lz * 2;
  const crown = g + height + 3;
  put(cx, cz, crown - 2, crown - 1, B.ACACIA_LOG);
  for (let dz = -3; dz <= 3; dz++) {
    for (let dx = -3; dx <= 3; dx++) {
      const ax = Math.abs(dx);
      const az = Math.abs(dz);
      if (ax + az > 4) continue;
      const inner = ax < 3 && az < 3 && ax + az < 4;
      put(cx + dx, cz + dz, crown - 1, crown + (inner ? 1 : 0), B.ACACIA_LEAVES);
    }
  }
}

function cherry({ ground, put, seed }: Plot, x: number, z: number, height: number): void {
  const g = ground[z * GRID + x]!;
  const radius = 3 + (hash(x, z, seed + 33) > 0.5 ? 1 : 0);
  put(x, z, g, g + height, B.CHERRY_LOG);
  for (let dz = -radius; dz <= radius; dz++) {
    for (let dx = -radius; dx <= radius; dx++) {
      const d = Math.hypot(dx, dz);
      if (d > radius + 0.3) continue;
      const rim = d > radius - 1.2 ? 1 : 0;
      put(x + dx, z + dz, g + height - 1 + rim, g + height + 2 - rim + (d < 1.5 ? 1 : 0), B.CHERRY_LEAVES);
    }
  }
}

/** A tree of `kind` at a column: `size` in 0..1 is how tall it grew, and `snowy` whether snow lies on it. */
export function grow(plot: Plot, kind: Tree, x: number, z: number, size: number, snowy: boolean): void {
  const g = plot.ground[z * GRID + x]!;
  if (kind === 'oak') oak(plot, x, z, 4 + Math.floor(size * 3), B.OAK_LEAVES, B.OAK_LOG);
  else if (kind === 'swamp oak') oak(plot, x, z, 5 + Math.floor(size * 3), B.OAK_LEAVES, B.OAK_LOG, 3);
  else if (kind === 'tall oak') tallOak(plot, x, z, 7 + Math.floor(size * 5));
  else if (kind === 'birch') oak(plot, x, z, 5 + Math.floor(size * 3), B.BIRCH_LEAVES, B.BIRCH_LOG);
  else if (kind === 'spruce') spruce(plot, x, z, 6 + Math.floor(size * 5), snowy ? B.SNOWY_LEAVES : B.SPRUCE_LEAVES);
  else if (kind === 'dark oak') darkOak(plot, x, z, 6 + Math.floor(size * 3));
  else if (kind === 'acacia') acacia(plot, x, z, 4 + Math.floor(size * 2));
  else if (kind === 'cherry') cherry(plot, x, z, 4 + Math.floor(size * 2));
  else plot.put(x, z, g, g + 2 + Math.floor(size * 2), B.CACTUS);
}
