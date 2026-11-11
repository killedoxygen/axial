/**
 * A piece of wild terrain built from a seed. A biome decides which blocks, trees and plants belong together; a scene
 * decides the lie of the land and where the camera stands in it.
 */
import * as T from './textures';

export const GRID = 208;
/** Extra runs a column may carry above its ground: a trunk, foliage layers, a plant. */
export const RUNS = 5;

let count = 1;
export const GRASS = count++;
export const DIRT = count++;
export const PODZOL = count++;
export const STONE = count++;
export const GRAVEL = count++;
export const SAND = count++;
export const RED_SAND = count++;
export const SNOWY_GRASS = count++;
export const SNOW = count++;
export const ICE = count++;
export const OBSIDIAN = count++;
export const CRYING = count++;
export const NETHERRACK = count++;
export const MAGMA = count++;
/** Banded clay: every level of the world has its own colour. */
export const TERRACOTTA = count++;
export const OAK_LOG = count++;
export const BIRCH_LOG = count++;
export const SPRUCE_LOG = count++;
export const DARK_OAK_LOG = count++;
export const ACACIA_LOG = count++;
export const CHERRY_LOG = count++;
export const CACTUS = count++;
/** From here to `LAVA`, foliage: seen through where its texture has gaps. */
export const FOLIAGE = count;
export const OAK_LEAVES = count++;
export const BIRCH_LEAVES = count++;
export const SPRUCE_LEAVES = count++;
/** Spruce foliage with snow lying on it. */
export const SNOWY_LEAVES = count++;
export const DARK_OAK_LEAVES = count++;
export const ACACIA_LEAVES = count++;
export const CHERRY_LEAVES = count++;
/** Hangs from foliage as a curtain: it has sides and no top. */
export const VINE = count++;
export const LAVA = count++;
export const WATER = count++;
/** From here to `DECAL`, plants: upright cut-outs that cast no shadow. */
export const PLANT = count;
export const SHORT_GRASS = count++;
export const TALL_GRASS = count++;
export const FERN = count++;
export const POPPY = count++;
export const DANDELION = count++;
export const CORNFLOWER = count++;
export const DAISY = count++;
export const BLUET = count++;
export const ALLIUM = count++;
export const ORCHID = count++;
export const DEAD_BUSH = count++;
export const CANE = count++;
export const RED_CAP = count++;
export const BROWN_CAP = count++;
/** From here up, what lies flat on the ground or the water. */
export const DECAL = count;
export const LILY = count++;
export const PETALS = count++;
export const MATERIALS = count;

/** Texture per material for its top, side and underside; for a plant, its upper and lower block. */
export const FACES = new Uint8Array(MATERIALS * 3);
/** Texture per ground material of the soil just under its surface block, and of the rock under that. */
export const BELOW = new Uint8Array(MATERIALS * 2);
const faces = (mat: number, top: number, side = top, under = side): void => FACES.set([top, side, under], mat * 3);
const below = (mat: number, soil: number, rock: number = T.STONE_TEX): void => BELOW.set([soil, rock], mat * 2);
faces(GRASS, T.GRASS_TOP, T.GRASS_SIDE, T.DIRT_TEX);
faces(DIRT, T.DIRT_TEX);
faces(PODZOL, T.PODZOL_TOP, T.PODZOL_SIDE, T.DIRT_TEX);
faces(STONE, T.STONE_TEX);
faces(GRAVEL, T.GRAVEL_TEX);
faces(SAND, T.SAND_TEX);
faces(RED_SAND, T.RED_SAND_TEX);
faces(SNOWY_GRASS, T.SNOW_TEX, T.SNOW_SIDE, T.DIRT_TEX);
faces(SNOW, T.SNOW_TEX);
faces(ICE, T.ICE_TEX);
faces(OBSIDIAN, T.OBSIDIAN_TEX);
faces(CRYING, T.CRYING_TEX);
faces(NETHERRACK, T.NETHERRACK_TEX);
faces(MAGMA, T.MAGMA_TEX);
faces(TERRACOTTA, T.CLAY_PLAIN);
faces(OAK_LOG, T.OAK_TOP, T.OAK_SIDE, T.OAK_TOP);
faces(BIRCH_LOG, T.BIRCH_TOP, T.BIRCH_SIDE, T.BIRCH_TOP);
faces(SPRUCE_LOG, T.SPRUCE_TOP, T.SPRUCE_SIDE, T.SPRUCE_TOP);
faces(DARK_OAK_LOG, T.DARK_OAK_TOP, T.DARK_OAK_SIDE, T.DARK_OAK_TOP);
faces(ACACIA_LOG, T.ACACIA_TOP, T.ACACIA_SIDE, T.ACACIA_TOP);
faces(CHERRY_LOG, T.CHERRY_TOP, T.CHERRY_SIDE, T.CHERRY_TOP);
faces(CACTUS, T.CACTUS_TOP, T.CACTUS_SIDE);
faces(OAK_LEAVES, T.OAK_LEAVES_TEX);
faces(BIRCH_LEAVES, T.BIRCH_LEAVES_TEX);
faces(SPRUCE_LEAVES, T.SPRUCE_LEAVES_TEX);
faces(SNOWY_LEAVES, T.SNOW_TEX, T.SPRUCE_LEAVES_TEX);
faces(DARK_OAK_LEAVES, T.DARK_OAK_LEAVES_TEX);
faces(ACACIA_LEAVES, T.ACACIA_LEAVES_TEX);
faces(CHERRY_LEAVES, T.CHERRY_LEAVES_TEX);
faces(VINE, T.VINE_TEX);
faces(LAVA, T.LAVA_TEX);
faces(WATER, T.SAND_TEX);
faces(SHORT_GRASS, T.SHORT_GRASS_TEX);
faces(TALL_GRASS, T.TALL_GRASS_HIGH, T.TALL_GRASS_LOW);
faces(FERN, T.FERN_TEX);
faces(POPPY, T.POPPY_TEX);
faces(DANDELION, T.DANDELION_TEX);
faces(CORNFLOWER, T.CORNFLOWER_TEX);
faces(DAISY, T.DAISY_TEX);
faces(BLUET, T.BLUET_TEX);
faces(ALLIUM, T.ALLIUM_TEX);
faces(ORCHID, T.ORCHID_TEX);
faces(DEAD_BUSH, T.DEAD_BUSH_TEX);
faces(CANE, T.CANE_TEX);
faces(RED_CAP, T.RED_CAP_TEX);
faces(BROWN_CAP, T.BROWN_CAP_TEX);
faces(LILY, T.LILY_TEX);
faces(PETALS, T.PETALS_TEX);
for (let mat = 1; mat < FOLIAGE; mat++) below(mat, FACES[mat * 3 + 1]!, FACES[mat * 3 + 1]);
for (const mat of [GRASS, DIRT, PODZOL, SNOWY_GRASS]) below(mat, T.DIRT_TEX);
below(GRAVEL, T.GRAVEL_TEX);
below(SNOW, T.SNOW_TEX);
below(SAND, T.SAND_TEX, T.SANDSTONE_TEX);
// Under red sand lies the banded clay; the renderer reads plain clay as "the band of this level".
below(RED_SAND, T.RED_SAND_TEX, T.CLAY_PLAIN);

/** The light each material gives off, on the game's scale of 0 to 15. */
export const EMITS = new Uint8Array(MATERIALS);
EMITS[LAVA] = 15;
EMITS[CRYING] = 10;
EMITS[MAGMA] = 3;

/** The clay's colour by level, bottom to top, repeating. */
export const BANDS: readonly number[] = [
  T.CLAY_PLAIN,
  T.CLAY_PLAIN,
  T.CLAY_ORANGE,
  T.CLAY_ORANGE,
  T.CLAY_YELLOW,
  T.CLAY_PLAIN,
  T.CLAY_BROWN,
  T.CLAY_RED,
  T.CLAY_RED,
  T.CLAY_ORANGE,
  T.CLAY_WHITE,
  T.CLAY_PLAIN,
  T.CLAY_GREY,
  T.CLAY_ORANGE,
  T.CLAY_ORANGE,
  T.CLAY_PLAIN,
  T.CLAY_YELLOW,
  T.CLAY_RED,
  T.CLAY_PLAIN,
  T.CLAY_WHITE,
  T.CLAY_BROWN,
  T.CLAY_ORANGE,
];

export type Rgb = readonly [number, number, number];

export interface World {
  seed: number;
  ground: Uint8Array;
  groundMat: Uint8Array;
  /** Per column: `runCount` runs of (bottom, top, material), bottom to top. */
  runs: Uint8Array;
  runCount: Uint8Array;
  /** Per column, what lies flat on its ground or water: a `DECAL` material, or 0. */
  decal: Uint8Array;
  /** Highest shadow-casting block per column. */
  top: Uint8Array;
  /** The water's level, or 0 in a world with none. */
  waterLevel: number;
  /** Per column, the light from glowing blocks and from the sky at its surface, on the game's scale of 0 to 15. */
  blockLight: Uint8Array;
  skyLight: Uint8Array;
  atlas: T.Atlas;
  /** The time of day as a turn of the sky: 0 at sunrise, a quarter at noon, a half at sunset. */
  time: number;
  /** Unit vector toward the sun, which is under the horizon at night; the moon stands opposite. */
  sun: readonly [number, number, number];
  camera: { x: number; y: number; z: number; yaw: number };
}

function hash2(x: number, z: number, seed: number): number {
  let h = Math.imul(x, 0x27d4eb2d) ^ Math.imul(z, 0x165667b1) ^ Math.imul(seed, 0x9e3779b9);
  h = Math.imul(h ^ (h >>> 15), 0x85ebca6b);
  h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35);
  return ((h ^ (h >>> 16)) >>> 0) / 4294967296;
}

function smooth(t: number): number {
  const c = t < 0 ? 0 : t > 1 ? 1 : t;
  return c * c * (3 - 2 * c);
}

export function valueNoise(x: number, z: number, seed: number): number {
  const xi = Math.floor(x);
  const zi = Math.floor(z);
  const tx = smooth(x - xi);
  const tz = smooth(z - zi);
  const a = hash2(xi, zi, seed);
  const b = hash2(xi + 1, zi, seed);
  const c = hash2(xi, zi + 1, seed);
  const d = hash2(xi + 1, zi + 1, seed);
  return a + (b - a) * tx + (c - a) * tz + (a - b - c + d) * tx * tz;
}

function fbm(x: number, z: number, seed: number): number {
  return (
    valueNoise(x, z, seed) * 0.55 +
    valueNoise(x * 2.1, z * 2.1, seed + 17) * 0.3 +
    valueNoise(x * 4.3, z * 4.3, seed + 31) * 0.15
  );
}

/** Mulberry32: a small seeded generator, so one seed always builds the same world. */
function random(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

type Tree = 'oak' | 'tall oak' | 'swamp oak' | 'birch' | 'spruce' | 'dark oak' | 'acacia' | 'cherry' | 'cactus';

/** What each kind of tree is made of. */
const WOOD: Record<Tree, number[]> = {
  oak: [OAK_LOG, OAK_LEAVES],
  'tall oak': [OAK_LOG, OAK_LEAVES],
  'swamp oak': [OAK_LOG, OAK_LEAVES, VINE],
  birch: [BIRCH_LOG, BIRCH_LEAVES],
  spruce: [SPRUCE_LOG, SPRUCE_LEAVES],
  'dark oak': [DARK_OAK_LOG, DARK_OAK_LEAVES],
  acacia: [ACACIA_LOG, ACACIA_LEAVES],
  cherry: [CHERRY_LOG, CHERRY_LEAVES],
  cactus: [CACTUS],
};

/**
 * The lie of the land: along a river, on a lake's shore, on a coast, in open country, under mountains, or in a
 * marsh of shallow pools.
 */
type Scene = 'river' | 'lake' | 'coast' | 'open' | 'peaks' | 'marsh';

interface Biome {
  surface: number;
  /** The ground at the water's edge, and under the water. */
  shore: number;
  bed: number;
  trees: [kind: Tree, share: number][];
  density: number;
  /** Rolling of the land, in blocks. */
  relief: number;
  plants: [material: number, chance: number][];
  /** The biome's grass and foliage colours (sRGB), which tint the grey textures. */
  grass: number;
  foliage: number;
  water: number;
  scenes: Scene[];
  /** How likely a lava spring is. */
  lava: number;
  frozen?: boolean;
  /** High ground turns to bare stone, then snow. */
  alpine?: boolean;
  /** The land rises in flat-topped steps of banded clay. */
  mesa?: boolean;
  /** Chances of sugar cane on a bank, lily pads on shallow water, fallen blossom on grass. */
  cane?: number;
  lilies?: number;
  petals?: number;
  /** A second surface in patches, as leaf litter lies under old spruce. */
  patches?: number;
}

const BIOMES: Biome[] = [
  // Plains.
  {
    surface: GRASS,
    shore: SAND,
    bed: SAND,
    trees: [
      ['oak', 5],
      ['tall oak', 1],
    ],
    density: 0.04,
    relief: 5,
    plants: [
      [SHORT_GRASS, 0.4],
      [TALL_GRASS, 0.05],
      [DANDELION, 0.012],
      [POPPY, 0.01],
      [BLUET, 0.008],
      [DAISY, 0.008],
      [CORNFLOWER, 0.006],
    ],
    grass: 0x91bd59,
    foliage: 0x77ab2f,
    water: 0x3f76e4,
    scenes: ['open', 'river', 'lake', 'coast'],
    lava: 0.25,
    cane: 0.1,
  },
  // Forest.
  {
    surface: GRASS,
    shore: SAND,
    bed: SAND,
    trees: [
      ['oak', 4],
      ['birch', 2],
      ['tall oak', 0.4],
    ],
    density: 0.5,
    relief: 7,
    plants: [
      [SHORT_GRASS, 0.26],
      [TALL_GRASS, 0.02],
      [POPPY, 0.012],
      [DANDELION, 0.014],
    ],
    grass: 0x79c05a,
    foliage: 0x59ae30,
    water: 0x3f76e4,
    scenes: ['river', 'lake', 'peaks', 'coast'],
    lava: 0.3,
    alpine: true,
    cane: 0.06,
  },
  // Birch forest.
  {
    surface: GRASS,
    shore: SAND,
    bed: GRAVEL,
    trees: [['birch', 1]],
    density: 0.42,
    relief: 6,
    plants: [
      [SHORT_GRASS, 0.3],
      [DAISY, 0.01],
      [DANDELION, 0.01],
      [ALLIUM, 0.006],
    ],
    grass: 0x88bb67,
    foliage: 0x6ba941,
    water: 0x3f76e4,
    scenes: ['river', 'lake', 'open'],
    lava: 0.2,
  },
  // Dark forest.
  {
    surface: GRASS,
    shore: GRASS,
    bed: GRAVEL,
    trees: [
      ['dark oak', 8],
      ['oak', 1.5],
      ['birch', 0.5],
    ],
    density: 0.6,
    relief: 5,
    plants: [
      [SHORT_GRASS, 0.14],
      [BROWN_CAP, 0.014],
      [RED_CAP, 0.012],
    ],
    grass: 0x507a32,
    foliage: 0x59ae30,
    water: 0x3f76e4,
    scenes: ['river', 'lake'],
    lava: 0.3,
  },
  // Taiga.
  {
    surface: GRASS,
    shore: GRASS,
    bed: GRAVEL,
    trees: [['spruce', 1]],
    density: 0.55,
    relief: 9,
    plants: [
      [FERN, 0.2],
      [SHORT_GRASS, 0.14],
      [BROWN_CAP, 0.005],
    ],
    grass: 0x86b783,
    foliage: 0x68a464,
    water: 0x3f76e4,
    scenes: ['river', 'lake', 'peaks'],
    lava: 0.35,
    alpine: true,
    patches: PODZOL,
  },
  // Snowy taiga, its water frozen.
  {
    surface: SNOWY_GRASS,
    shore: SNOWY_GRASS,
    bed: DIRT,
    trees: [['spruce', 1]],
    density: 0.36,
    relief: 8,
    plants: [],
    grass: 0x80b497,
    foliage: 0x60a17b,
    water: 0x3938c9,
    scenes: ['river', 'lake', 'peaks'],
    lava: 0.3,
    frozen: true,
    alpine: true,
  },
  // Savanna.
  {
    surface: GRASS,
    shore: SAND,
    bed: SAND,
    trees: [
      ['acacia', 6],
      ['oak', 1],
    ],
    density: 0.12,
    relief: 6,
    plants: [
      [SHORT_GRASS, 0.5],
      [TALL_GRASS, 0.12],
    ],
    grass: 0xbfb755,
    foliage: 0xaea42a,
    water: 0x3f76e4,
    scenes: ['open', 'river', 'lake'],
    lava: 0.3,
  },
  // Desert.
  {
    surface: SAND,
    shore: SAND,
    bed: SAND,
    trees: [['cactus', 1]],
    density: 0.05,
    relief: 6,
    plants: [[DEAD_BUSH, 0.012]],
    grass: 0xbfb755,
    foliage: 0xaea42a,
    water: 0x3f76e4,
    scenes: ['open', 'river', 'coast'],
    lava: 0.5,
    cane: 0.12,
  },
  // Badlands.
  {
    surface: RED_SAND,
    shore: RED_SAND,
    bed: RED_SAND,
    trees: [['cactus', 1]],
    density: 0.03,
    relief: 26,
    plants: [[DEAD_BUSH, 0.02]],
    grass: 0x90814d,
    foliage: 0x9e814d,
    water: 0x3f76e4,
    scenes: ['river', 'open'],
    lava: 0.5,
    mesa: true,
  },
  // Swamp.
  {
    surface: GRASS,
    shore: GRASS,
    bed: DIRT,
    trees: [['swamp oak', 1]],
    density: 0.13,
    relief: 2,
    plants: [
      [SHORT_GRASS, 0.3],
      [ORCHID, 0.012],
      [BROWN_CAP, 0.008],
      [DEAD_BUSH, 0.004],
    ],
    grass: 0x6a7039,
    foliage: 0x6a7039,
    water: 0x617b64,
    scenes: ['marsh'],
    lava: 0,
    cane: 0.12,
    lilies: 0.1,
  },
  // Cherry grove.
  {
    surface: GRASS,
    shore: GRASS,
    bed: GRAVEL,
    trees: [
      ['cherry', 6],
      ['birch', 1],
    ],
    density: 0.36,
    relief: 8,
    plants: [
      [SHORT_GRASS, 0.3],
      [DANDELION, 0.012],
    ],
    grass: 0xb6db61,
    foliage: 0xb6db61,
    water: 0x5db7ef,
    scenes: ['peaks', 'lake', 'open'],
    lava: 0.15,
    alpine: true,
    petals: 0.3,
  },
  // Meadow under mountains.
  {
    surface: GRASS,
    shore: GRASS,
    bed: GRAVEL,
    trees: [
      ['oak', 2],
      ['birch', 1],
    ],
    density: 0.015,
    relief: 7,
    plants: [
      [SHORT_GRASS, 0.42],
      [TALL_GRASS, 0.05],
      [CORNFLOWER, 0.03],
      [ALLIUM, 0.025],
      [DAISY, 0.03],
      [BLUET, 0.03],
      [POPPY, 0.03],
      [DANDELION, 0.03],
    ],
    grass: 0x83bb6d,
    foliage: 0x63a948,
    water: 0x0e4ecf,
    scenes: ['peaks', 'open', 'lake'],
    lava: 0.15,
    alpine: true,
  },
];

/**
 * An hour to draw the world at, and where its light stands from straight ahead: the sun by day, the moon by night.
 * Past a quarter turn the light is behind the camera.
 */
interface Hour {
  time: number;
  bearing: number;
}

const DARK_HOURS: Hour[] = [
  // The last of the sun, low ahead.
  { time: 0.512, bearing: 0.06 },
  // The same hour with the sun to one side.
  { time: 0.512, bearing: 0.55 },
  // Afterglow, the sun on the horizon.
  { time: 0.527, bearing: 0.08 },
  // Night, the moon low ahead.
  { time: 0.6, bearing: -0.3 },
  // Deep night, the moon high behind.
  { time: 0.72, bearing: 2.5 },
];

const LIGHT_HOURS: Hour[] = [
  // Morning, the sun ahead.
  { time: 0.05, bearing: 0.1 },
  // Noon, the sun over the shoulder.
  { time: 0.25, bearing: 2.3 },
  // Afternoon, the sun to one side.
  { time: 0.38, bearing: -1.2 },
  // Golden hour, the sun low to one side.
  { time: 0.44, bearing: 0.55 },
];

/**
 * @param seed The instance's art seed.
 * @param dark Whether the app is in its dark mode: sunset to night, or daylight.
 * @param pack The instance's own block textures. With too few of them to build any world there is none.
 */
export function buildWorld(seed: number, dark: boolean, pack: T.TexturePack): World | null {
  const n = GRID;
  const next = random(seed);
  const has = (mat: number): boolean =>
    T.hasTexture(pack, FACES[mat * 3]!) &&
    T.hasTexture(pack, FACES[mat * 3 + 1]!) &&
    T.hasTexture(pack, FACES[mat * 3 + 2]!);
  // A version only shows what it has: a biome needs its ground and at least one of its trees.
  const livable = BIOMES.map((biome) => ({
    ...biome,
    trees: biome.trees.filter(([kind]) => WOOD[kind].every(has)),
    plants: biome.plants.filter(([plant]) => has(plant)),
    patches: biome.patches && has(biome.patches) ? biome.patches : undefined,
    lilies: has(LILY) ? biome.lilies : undefined,
    petals: has(PETALS) ? biome.petals : undefined,
    cane: has(CANE) ? biome.cane : undefined,
  })).filter(
    (biome) =>
      biome.trees.length > 0 &&
      [biome.surface, biome.shore, biome.bed].every(has) &&
      (!biome.mesa || BANDS.every((band) => T.hasTexture(pack, band))) &&
      (!biome.frozen || (has(ICE) && has(SNOW))),
  );
  if (livable.length === 0 || ![STONE, DIRT, GRAVEL, SAND].every(has)) return null;
  const biome = livable[Math.floor(next() * livable.length)]!;
  const scene = biome.scenes[Math.floor(next() * biome.scenes.length)]!;
  const hours = dark ? DARK_HOURS : LIGHT_HOURS;
  const hour = hours[Math.floor(next() * hours.length)]!;

  const ground = new Uint8Array(n * n);
  const groundMat = new Uint8Array(n * n);
  const runs = new Uint8Array(n * n * RUNS * 3);
  const runCount = new Uint8Array(n * n);
  const decal = new Uint8Array(n * n);
  const top = new Uint8Array(n * n);
  const sea = 14;

  // Every scene is two fields: how far a point lies inland of the water's edge (negative under water), and how
  // much the land climbs there beyond its rolling. The camera stands near z = 8 and looks up the grid.
  const mid = n / 2;
  const phase = next() * 6.28;
  const side = next() < 0.5 ? -1 : 1;
  const pond = next() < 0.6;
  const stream = next() < 0.6;
  const cameraZ = 8;
  const riverX = (z: number): number =>
    mid + 6 + Math.sin(z / 40 + phase) * 7 + (valueNoise(z / 14, 3, seed) - 0.5) * 6;
  const riverHalf = (z: number): number => 6 + z * 0.14 + valueNoise(z / 18, 9, seed) * 5;
  const coastX = (z: number): number =>
    mid + 5 + Math.sin(z / 34 + phase) * 7 + (valueNoise(z / 12, 5, seed) - 0.5) * 8;
  const valleyX = (z: number): number => mid + Math.sin(z / 45 + phase) * 10;
  const far = (x: number, z: number): number => smooth(fbm(x / 40, z / 40, seed + 5) * 2.2 - 0.5);
  let inland: (x: number, z: number) => number;
  let climb: (x: number, z: number, d: number) => number;
  /** The water's depth at its deepest, and how far from the edge it gets there. */
  let basin: [depth: number, reach: number] = [3.2, 6];
  let standX = mid;
  let aimX = mid;
  if (scene === 'river') {
    inland = (x, z) => {
      const half = riverHalf(z);
      return Math.abs(x - riverX(z)) - half + (valueNoise(x / 7, z / 7, seed + 9) - 0.5) * 0.3 * half;
    };
    climb = (x, z, d) => Math.max(0, d - 16) * 0.16 * far(x, z);
    standX = riverX(cameraZ) - riverHalf(cameraZ) - 4;
    aimX = riverX(70);
  } else if (scene === 'lake') {
    const lakeX = mid + 6 * side;
    inland = (x, z) => Math.hypot((x - lakeX) / 1.2, z - 62) - 44 - (valueNoise(x / 16, z / 16, seed + 9) - 0.5) * 24;
    climb = (x, z, d) =>
      Math.max(0, d - 8) * 0.24 * far(x, z) + smooth((z - 110) / 60) * 16 * fbm(x / 30, z / 30, seed + 7);
    basin = [5, 14];
    standX = mid - 5 * side;
    aimX = lakeX;
  } else if (scene === 'coast') {
    // Land on one side, open sea on the other and ahead.
    inland = (x, z) =>
      (coastX(z) - x) * side + (valueNoise(x / 9, z / 9, seed + 9) - 0.5) * 4 - Math.max(0, z - 120) * 0.5;
    climb = (x, z, d) => Math.max(0, d - 14) * 0.22 * far(x, z);
    basin = [9, 26];
    standX = coastX(cameraZ) - 6 * side;
    aimX = coastX(70) + 10 * side;
  } else if (scene === 'peaks') {
    inland = (x, z) => (stream ? Math.abs(x - valleyX(z)) - 2.2 + (valueNoise(x / 6, z / 6, seed + 9) - 0.5) * 2 : 99);
    // The mountains stand back from the valley floor.
    climb = (x, z, d) =>
      Math.max(0, Math.abs(x - valleyX(z)) - 18) * 0.3 * (0.4 + fbm(x / 28, z / 28, seed + 5)) +
      smooth((z - 64) / 90) * smooth(d / 45) * 62 * (0.25 + fbm(x / 34, z / 34, seed + 7));
    basin = [1.6, 2];
    standX = valleyX(cameraZ) - 5 * side;
    aimX = valleyX(70);
  } else if (scene === 'marsh') {
    inland = (x, z) =>
      (valueNoise(x / 9, z / 9, seed + 9) * 0.6 + valueNoise(x / 24, z / 24, seed + 11) * 0.6 - 0.64) * 16;
    climb = () => 0;
    basin = [1.8, 3];
    aimX = mid + 8 * side;
  } else {
    const pondX = mid + 13 * side;
    inland = (x, z) =>
      pond ? Math.hypot(x - pondX, (z - 34) * 1.3) - 7 + (valueNoise(x / 5, z / 5, seed + 9) - 0.5) * 4 : 99;
    climb = (x, z) =>
      (fbm(x / 50, z / 50, seed + 5) - 0.4) * 10 +
      smooth((z - 95) / 80) * (biome.alpine ? 44 : 18) * (0.2 + fbm(x / 34, z / 34, seed + 7));
    basin = [2.2, 5];
    aimX = mid - 6 * side;
  }
  const cameraX = Math.round(standX) + 0.5;
  const heading = Math.atan2(aimX - cameraX, 62);
  const beach = scene === 'coast' ? 7 : 2.5;
  const flat = scene === 'marsh' ? 0 : 1;

  for (let z = 0; z < n; z++) {
    for (let x = 0; x < n; x++) {
      const i = z * n + x;
      const stand = Math.hypot(x - cameraX, z - cameraZ);
      // Dry footing where the camera stands.
      const d = Math.max(inland(x, z), 4 - stand);
      let height: number;
      if (d < 0) height = sea - 1.2 - smooth(-d / basin[1]) * basin[0];
      else {
        const rolling = (fbm(x / 26, z / 26, seed) - 0.35) * biome.relief;
        // The land lies low round the camera and gathers height with distance, so the view opens out.
        const open = 0.3 + 0.7 * smooth((stand - 5) / 36);
        height =
          sea +
          0.6 +
          smooth(d / 0.6) * 1.2 * flat +
          smooth(d / 10) * (1.6 * flat + (Math.max(-1, rolling) + climb(x, z, d)) * open);
        // Flat-topped steps with sheer sides.
        if (biome.mesa && height > sea + 5) height = sea + 5 + Math.floor((height - sea - 5) / 8) * 9;
        // The camera stands on a slight rise.
        const knoll = sea + (flat ? 5.5 : 2);
        if (stand < 9) height = knoll + (height - knoll) * smooth(stand / 9);
      }
      const h = Math.max(3, Math.min(200, Math.round(height)));
      ground[i] = h;
      const rough = valueNoise(x / 6, z / 6, seed + 13);
      const pebbly = valueNoise(x / 13, z / 13, seed + 3) > 0.6;
      if (h < sea) groundMat[i] = pebbly && biome.bed !== RED_SAND ? GRAVEL : biome.bed;
      else if (h <= sea + 2 && d < beach && (scene === 'coast' || valueNoise(x / 12, z / 12, seed + 19) > 0.5))
        groundMat[i] = biome.shore;
      else if (biome.alpine && h > sea + (biome.frozen ? 22 : 46) + rough * 8) groundMat[i] = SNOW;
      else if (biome.alpine && h > sea + 28 + rough * 10) groundMat[i] = STONE;
      else if (biome.mesa && h > sea + 5) groundMat[i] = TERRACOTTA;
      else if (biome.patches && valueNoise(x / 8, z / 8, seed + 15) > 0.56) groundMat[i] = biome.patches;
      else groundMat[i] = biome.surface;
    }
  }

  const addRun = (x: number, z: number, y0: number, y1: number, mat: number): void => {
    if (x < 0 || z < 0 || x >= n || z >= n || y1 <= y0 || y0 < ground[z * n + x]!) return;
    const i = z * n + x;
    const count = runCount[i]!;
    const base = i * RUNS * 3;
    // Runs stay sorted bottom to top. One that meets a run of its own kind joins it; one that would overlap
    // anything else is dropped.
    let at = count;
    while (at > 0 && runs[base + (at - 1) * 3]! >= y1) at--;
    if (at > 0 && runs[base + (at - 1) * 3 + 1]! >= y0) {
      const o = base + (at - 1) * 3;
      if (runs[o + 2] === mat && runs[o]! <= y0) runs[o + 1] = Math.max(runs[o + 1]!, y1);
      if (runs[o + 2] === mat || runs[o + 1]! > y0) return;
    }
    if (count >= RUNS) return;
    for (let k = count; k > at; k--) runs.copyWithin(base + k * 3, base + k * 3 - 3, base + k * 3);
    runs.set([y0, Math.min(255, y1), mat], base + at * 3);
    runCount[i] = count + 1;
  };

  // A light the game itself would put there, always in the same part of the picture and by the water where there
  // is any: a pool of lava sunk in stone, or a ruined portal on its patch of netherrack.
  let gladeX = -99;
  let gladeZ = -99;
  if (has(LAVA) && (dark || next() < biome.lava)) {
    // The nearest shore-side land in the part of the view right of centre.
    const wanted = 14 + next() * 6;
    let best = Infinity;
    for (let z = cameraZ; z < cameraZ + 110; z++) {
      for (let x = 8; x < n - 8; x++) {
        const ahead = (z - cameraZ) * Math.cos(heading) + (x - cameraX) * Math.sin(heading);
        const aside = (x - cameraX) * Math.cos(heading) - (z - cameraZ) * Math.sin(heading);
        if (ahead < 12 || aside < ahead * 0.15 || aside > ahead * 0.75 || ground[z * n + x]! <= sea) continue;
        const score = Math.abs(ahead - wanted) + Math.abs(inland(x, z) - 5) * 0.6;
        if (score < best) {
          best = score;
          gladeX = x;
          gladeZ = z;
        }
      }
    }
    if (best < Infinity) {
      // The pool lies in the lowest ground it covers, so it sinks into the land and never stands above it.
      let foot = 255;
      for (let dz = -3; dz <= 3; dz++)
        for (let dx = -3; dx <= 3; dx++)
          foot = Math.min(foot, Math.max(sea + 1, ground[(gladeZ + dz) * n + gladeX + dx]!));
      // By night a standing frame shows where a pool lying flat would not.
      const portal = has(OBSIDIAN) && has(NETHERRACK) && has(MAGMA) && next() < (hour.time > 0.55 ? 0.8 : 0.35);
      for (let dz = -8; dz <= 8; dz++) {
        for (let dx = -8; dx <= 8; dx++) {
          const i = (gladeZ + dz) * n + gladeX + dx;
          const rough = hash2(gladeX + dx, gladeZ + dz, seed + 21);
          const d = Math.hypot(dx, dz * 1.3) + rough;
          if (d > 8) continue;
          // Stone holds the pool in where the land or the water beside it lies lower.
          if (d < 5.6 && ground[i]! < foot) {
            ground[i] = foot;
            groundMat[i] = STONE;
          }
          if (ground[i]! < sea) continue;
          // The land about it eases down to its level, so nothing walls the light in.
          const most = foot + (d < 5 ? 0 : Math.ceil((d - 5) * 0.5));
          ground[i] = d < 5 ? foot : Math.min(ground[i]!, most);
          if (portal) {
            if (d < 4.6) groundMat[i] = rough > 0.8 ? LAVA : rough > 0.62 ? MAGMA : NETHERRACK;
          } else if (d < 4.4) groundMat[i] = LAVA;
          else if (d < 5.6) groundMat[i] = STONE;
        }
      }
      // Lava lies in the ground: where the land falls away beside it, stone holds it in.
      for (let dz = -4; dz <= 4; dz++) {
        for (let dx = -4; dx <= 4; dx++) {
          const i = (gladeZ + dz) * n + gladeX + dx;
          if (groundMat[i] !== LAVA) continue;
          if (Math.min(ground[i - 1]!, ground[i + 1]!, ground[i - n]!, ground[i + n]!) < ground[i]!)
            groundMat[i] = portal ? NETHERRACK : STONE;
        }
      }
      if (portal) {
        // The frame: two pillars and a lintel, some of it fallen, some of it weeping.
        const block = (x: number, y: number): void => {
          const roll = hash2(x, y, seed + 23);
          if (roll > 0.14) addRun(x, gladeZ, y, y + 1, has(CRYING) && roll > 0.74 ? CRYING : OBSIDIAN);
        };
        for (let dx = -2; dx <= 1; dx++) {
          const x = gladeX + dx;
          const g = ground[gladeZ * n + x]!;
          groundMat[gladeZ * n + x] = OBSIDIAN;
          if (dx === -2 || dx === 1) for (let y = g; y < g + 5; y++) block(x, y);
          else block(x, foot + 4);
        }
      }
    }
  }

  /** The game's small tree: two wide layers of foliage, then two narrow ones, the top a plus. */
  const oak = (x: number, z: number, height: number, leaf: number, log: number, spread = 2): void => {
    const g = ground[z * n + x]!;
    const crown = g + height;
    addRun(x, z, g, crown, log);
    addRun(x, z, crown, crown + 2, leaf);
    for (let dz = -spread; dz <= spread; dz++) {
      for (let dx = -spread; dx <= spread; dx++) {
        const ax = Math.abs(dx);
        const az = Math.abs(dz);
        if (ax + az === 0) continue;
        const keep = hash2(x + dx, z + dz, seed + 31) > 0.45;
        const rim = ax === spread || az === spread;
        if (rim && ax === az && !keep) continue;
        const inner = ax < 2 && az < 2;
        addRun(x + dx, z + dz, crown - 2, crown + (!inner ? 0 : ax + az === 1 ? 2 : keep ? 1 : 0), leaf);
        // A swamp's oaks trail vines from the rim of the crown.
        if (spread > 2 && rim && hash2(x + dx, z + dz, seed + 35) > 0.45)
          addRun(x + dx, z + dz, crown - 3 - Math.floor(hash2(x + dx, z + dz, seed + 37) * 3), crown - 2, VINE);
      }
    }
  };

  /** A tall trunk carrying several rounded clumps of foliage. */
  const tallOak = (x: number, z: number, height: number): void => {
    const g = ground[z * n + x]!;
    const crown = g + height;
    addRun(x, z, g, crown, OAK_LOG);
    addRun(x, z, crown, crown + 3, OAK_LEAVES);
    for (let clump = 0; clump < 4; clump++) {
      const ox = clump === 0 ? 0 : Math.round((hash2(x + clump, z, seed + 61) - 0.5) * 6);
      const oz = clump === 0 ? 0 : Math.round((hash2(x, z + clump, seed + 63) - 0.5) * 6);
      const oy = crown - (clump === 0 ? 0 : 1 + clump);
      const radius = clump === 0 ? 2.7 : 2.2;
      for (let dz = -3; dz <= 3; dz++) {
        for (let dx = -3; dx <= 3; dx++) {
          const reach = radius * radius - dx * dx - dz * dz;
          if (reach < 0 || (ox + dx === 0 && oz + dz === 0)) continue;
          const half = Math.floor(Math.sqrt(reach) + 0.4);
          addRun(x + ox + dx, z + oz + dz, oy - half, oy + half + 1, OAK_LEAVES);
        }
      }
    }
  };

  const spruce = (x: number, z: number, height: number, leaf: number): void => {
    const g = ground[z * n + x]!;
    // From the tip down, the foliage steps out and in.
    const radii = [0, 1, 2, 1, 2, 3, 2, 3];
    const layers = Math.min(height - 1, radii.length);
    addRun(x, z, g, g + height, SPRUCE_LOG);
    addRun(x, z, g + height, g + height + 1, leaf);
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
            addRun(x + dx, z + dz, from, y, leaf);
            from = -1;
          }
        }
      }
    }
  };

  /** Four trunks side by side under one broad, deep crown. */
  const darkOak = (x: number, z: number, height: number): void => {
    const crown = ground[z * n + x]! + height;
    for (let dz = -3; dz <= 4; dz++) {
      for (let dx = -3; dx <= 4; dx++) {
        const d = Math.hypot(dx - 0.5, dz - 0.5);
        if (d < 1) {
          addRun(x + dx, z + dz, ground[(z + dz) * n + x + dx]!, crown, DARK_OAK_LOG);
          addRun(x + dx, z + dz, crown, crown + 2, DARK_OAK_LEAVES);
        } else if (d < 4.1 && (d < 3.4 || hash2(x + dx, z + dz, seed + 31) > 0.4))
          addRun(x + dx, z + dz, crown - 2, crown + (d < 2.2 ? 2 : d < 3.3 ? 1 : 0), DARK_OAK_LEAVES);
      }
    }
  };

  /** A trunk that leans over, then a wide flat crown two layers deep. */
  const acacia = (x: number, z: number, height: number): void => {
    const g = ground[z * n + x]!;
    const turn = Math.floor(hash2(x, z, seed + 65) * 4);
    const lx = turn === 0 ? 1 : turn === 1 ? -1 : 0;
    const lz = turn === 2 ? 1 : turn === 3 ? -1 : 0;
    addRun(x, z, g, g + height, ACACIA_LOG);
    addRun(x + lx, z + lz, g + height, g + height + 1, ACACIA_LOG);
    const cx = x + lx * 2;
    const cz = z + lz * 2;
    const crown = g + height + 3;
    addRun(cx, cz, crown - 2, crown - 1, ACACIA_LOG);
    for (let dz = -3; dz <= 3; dz++) {
      for (let dx = -3; dx <= 3; dx++) {
        const ax = Math.abs(dx);
        const az = Math.abs(dz);
        if (ax + az > 4) continue;
        const inner = ax < 3 && az < 3 && ax + az < 4;
        addRun(cx + dx, cz + dz, crown - 1, crown + (inner ? 1 : 0), ACACIA_LEAVES);
      }
    }
  };

  const cherry = (x: number, z: number, height: number): void => {
    const g = ground[z * n + x]!;
    const radius = 3 + (hash2(x, z, seed + 33) > 0.5 ? 1 : 0);
    addRun(x, z, g, g + height, CHERRY_LOG);
    for (let dz = -radius; dz <= radius; dz++) {
      for (let dx = -radius; dx <= radius; dx++) {
        const d = Math.hypot(dx, dz);
        if (d > radius + 0.3) continue;
        const rim = d > radius - 1.2 ? 1 : 0;
        addRun(x + dx, z + dz, g + height - 1 + rim, g + height + 2 - rim + (d < 1.5 ? 1 : 0), CHERRY_LEAVES);
      }
    }
  };

  let shares = 0;
  for (const [, share] of biome.trees) shares += share;
  const grow = (x: number, z: number, size: number, roll: number): void => {
    let kind = biome.trees[0]![0];
    let at = roll * shares;
    for (const [tree, share] of biome.trees) {
      kind = tree;
      if ((at -= share) < 0) break;
    }
    const g = ground[z * n + x]!;
    if (kind === 'oak') oak(x, z, 4 + Math.floor(size * 3), OAK_LEAVES, OAK_LOG);
    else if (kind === 'swamp oak') oak(x, z, 5 + Math.floor(size * 3), OAK_LEAVES, OAK_LOG, 3);
    else if (kind === 'tall oak') tallOak(x, z, 7 + Math.floor(size * 5));
    else if (kind === 'birch') oak(x, z, 5 + Math.floor(size * 3), BIRCH_LEAVES, BIRCH_LOG);
    else if (kind === 'spruce') spruce(x, z, 6 + Math.floor(size * 5), biome.frozen ? SNOWY_LEAVES : SPRUCE_LEAVES);
    else if (kind === 'dark oak') darkOak(x, z, 6 + Math.floor(size * 3));
    else if (kind === 'acacia') acacia(x, z, 4 + Math.floor(size * 2));
    else if (kind === 'cherry') cherry(x, z, 4 + Math.floor(size * 2));
    else addRun(x, z, g, g + 2 + Math.floor(size * 2), CACTUS);
  };

  const sandy = (mat: number): boolean => mat === SAND || mat === RED_SAND;
  const fertile = (mat: number): boolean => mat === GRASS || mat === PODZOL || mat === SNOWY_GRASS;
  /** Whether the ground of a column is bare and of a kind things grow in. A swamp's trees also stand in its shallows. */
  const bare = (i: number): boolean => {
    const first = runCount[i]! > 0 && runs[i * RUNS * 3] === ground[i];
    if (first) return false;
    if (ground[i]! < sea) return scene === 'marsh' && ground[i]! >= sea - 2;
    return fertile(groundMat[i]!) || sandy(groundMat[i]!) || groundMat[i] === TERRACOTTA;
  };
  const cactus = biome.trees[0]![0] === 'cactus';
  for (let z = 4; z < n - 5; z += 3) {
    for (let x = 4; x < n - 5; x += 3) {
      const jx = x + Math.floor(hash2(x, z, seed + 41) * 3);
      const jz = z + Math.floor(hash2(x, z, seed + 43) * 3);
      const i = jz * n + jx;
      if (!bare(i) || (sandy(groundMat[i]!) || groundMat[i] === TERRACOTTA) !== cactus) continue;
      // Keep the camera's footing, the view straight ahead of it and the water's edge clear.
      const ahead = (jz - cameraZ) * Math.cos(heading) + (jx - cameraX) * Math.sin(heading);
      const aside = Math.abs((jx - cameraX) * Math.cos(heading) - (jz - cameraZ) * Math.sin(heading));
      if (Math.hypot(jx - cameraX, jz - cameraZ) < 8 || (ahead > 0 && ahead < 32 && aside < 4 + ahead * 0.3)) continue;
      if (scene !== 'marsh' && inland(jx, jz) < 2) continue;
      if (Math.hypot(jx - gladeX, jz - gladeZ) < 7) continue;
      const thicket = valueNoise(jx / 15, jz / 15, seed + 51);
      if (hash2(jx, jz, seed + 47) > biome.density * (0.25 + thicket * 1.5)) continue;
      grow(jx, jz, hash2(jx, jz, seed + 59), hash2(jx, jz, seed + 53));
    }
  }
  const gladeReach = Math.hypot(gladeX - cameraX, gladeZ - cameraZ);
  for (let i = 0; i < n * n; i++) {
    const count = runCount[i]!;
    top[i] = count > 0 ? Math.max(ground[i]!, runs[(i * RUNS + count - 1) * 3 + 1]!) : ground[i]!;
  }

  // Undergrowth, after the shadow heights so it casts none. Flowers keep to drifts; cane to the banks.
  for (let z = 1; z < n - 1; z++) {
    for (let x = 1; x < n - 1; x++) {
      const i = z * n + x;
      const g = ground[i]!;
      if (g < sea) {
        if (biome.lilies && g >= sea - 2 && hash2(x, z, seed + 69) < biome.lilies) decal[i] = LILY;
        continue;
      }
      const close = Math.hypot(x - cameraX, z - cameraZ);
      if (!bare(i) || close < 3.5 || Math.hypot(x - gladeX, z - gladeZ) < 8) continue;
      // Nothing grows between the camera and its light.
      const along = ((x - cameraX) * (gladeX - cameraX) + (z - cameraZ) * (gladeZ - cameraZ)) / gladeReach;
      if (
        along > 0 &&
        along < gladeReach &&
        Math.abs((x - cameraX) * (gladeZ - cameraZ) - (z - cameraZ) * (gladeX - cameraX)) < gladeReach * 3.5
      )
        continue;
      const mat = groundMat[i]!;
      const bank =
        g <= sea + 1 && (ground[i - 1]! < sea || ground[i + 1]! < sea || ground[i - n]! < sea || ground[i + n]! < sea);
      if (bank && close > 6 && biome.cane && (fertile(mat) || sandy(mat)) && hash2(x, z, seed + 75) < biome.cane) {
        addRun(x, z, g, g + 1 + Math.floor(hash2(x, z, seed + 77) * 3), CANE);
        continue;
      }
      if (mat !== biome.surface && mat !== biome.patches && mat !== TERRACOTTA) continue;
      if (biome.petals && mat === GRASS && valueNoise(x / 6, z / 6, seed + 79) > 0.45 && hash2(x, z, seed + 81) < 0.6)
        decal[i] = PETALS;
      const drift = valueNoise(x / 9, z / 9, seed + 71) > 0.55 ? 2.6 : 0.3;
      const tuft = 0.3 + valueNoise(x / 11, z / 11, seed + 73);
      let roll = hash2(x, z, seed + 67);
      for (const [plant, chance] of biome.plants) {
        const odds = chance * (plant >= POPPY && plant <= ORCHID ? drift : plant <= FERN ? tuft : 1);
        if (roll < odds && (plant !== TALL_GRASS || close > 5)) {
          addRun(x, z, g, g + (plant === TALL_GRASS ? 2 : 1), plant);
          break;
        }
        roll -= odds;
      }
    }
  }
  let wet = false;
  for (let i = 0; i < n * n; i++) {
    if (ground[i]! >= sea) continue;
    wet = true;
    if (!biome.frozen) continue;
    ground[i] = sea;
    groundMat[i] = ICE;
    top[i] = Math.max(top[i]!, sea);
  }

  // Light as the game spreads it: one level lost for every block travelled, across or up and down. The sky's
  // light also thins by a level for each layer of foliage it passes.
  const blockLight = new Uint8Array(n * n);
  const skyLight = new Uint8Array(n * n);
  for (let i = 0; i < n * n; i++) {
    let glow = EMITS[groundMat[i]!]!;
    let shade = 0;
    for (let k = 0; k < runCount[i]!; k++) {
      const o = (i * RUNS + k) * 3;
      const mat = runs[o + 2]!;
      if (mat >= PLANT) continue;
      if (mat < FOLIAGE) shade = 15;
      else if (mat < LAVA) shade += runs[o + 1]! - runs[o]!;
      glow = Math.max(glow, EMITS[mat]!);
    }
    blockLight[i] = glow;
    skyLight[i] = Math.max(0, 15 - shade);
  }
  for (let pass = 0; pass < 4; pass++) {
    const forward = (pass & 1) === 0;
    for (let k = 0; k < n * n; k++) {
      const i = forward ? k : n * n - 1 - k;
      const x = i % n;
      const across = forward ? (x > 0 ? i - 1 : -1) : x < n - 1 ? i + 1 : -1;
      const along = forward ? i - n : i + n;
      for (const from of [across, along]) {
        if (from < 0 || from >= n * n) continue;
        const spread = blockLight[from]! - 1 - Math.abs(ground[from]! - ground[i]!);
        if (spread > blockLight[i]!) blockLight[i] = spread;
        if (skyLight[from]! - 1 > skyLight[i]!) skyLight[i] = skyLight[from]! - 1;
      }
    }
  }

  const yaw = heading + (next() - 0.5) * 0.06;
  // The sun's height follows from the hour; by night the hour's bearing is the moon's, opposite the sun.
  const turn = (((hour.time + 0.75) % 1) + 1) % 1;
  const arc = (turn + (0.5 - Math.cos(turn * Math.PI) * 0.5 - turn) / 3) * 2 * Math.PI;
  const height = Math.cos(arc) * 0.766;
  const bearing =
    yaw +
    hour.bearing * (hour.bearing > 0.2 && hour.bearing < 1.5 ? side : 1) +
    (height < 0 ? Math.PI : 0) +
    (next() - 0.5) * 0.08;
  const level = Math.sqrt(1 - height * height);
  return {
    seed,
    ground,
    groundMat,
    runs,
    runCount,
    decal,
    top,
    waterLevel: wet && !biome.frozen ? sea : 0,
    blockLight,
    skyLight,
    atlas: T.buildAtlas(pack, biome.grass, biome.foliage),
    time: hour.time,
    sun: [Math.sin(bearing) * level, height, Math.cos(bearing) * level],
    camera: { x: cameraX, y: ground[cameraZ * n + Math.floor(cameraX)]! + 1.62, z: cameraZ + 0.5, yaw },
  };
}
