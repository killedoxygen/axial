/**
 * What a world is made of: every material, the paints its faces take, and the light it shows.
 */
import * as T from './paints';

/** Runs a column may carry above its ground: a trunk and foliage, or a wall, its window and the roof over them. */
export const RUNS = 8;

let count = 1;
export const GRASS = count++;
export const DIRT = count++;
export const PODZOL = count++;
export const STONE = count++;
export const GRAVEL = count++;
export const SAND = count++;
export const SANDSTONE = count++;
export const RED_SAND = count++;
export const SNOWY_GRASS = count++;
export const SNOW = count++;
export const ICE = count++;
/** Banded clay: every level of the world has its own colour. */
export const TERRACOTTA = count++;
/** Fired clay of one colour, as roofs are tiled with. */
export const TILES = count++;
/** Pale clay, as a plastered wall is; and straw-coloured, as thatch is. */
export const PLASTER = count++;
export const THATCH = count++;
export const OAK_LOG = count++;
export const BIRCH_LOG = count++;
export const SPRUCE_LOG = count++;
export const DARK_OAK_LOG = count++;
export const ACACIA_LOG = count++;
export const CHERRY_LOG = count++;
export const OAK_PLANKS = count++;
export const BIRCH_PLANKS = count++;
export const SPRUCE_PLANKS = count++;
export const DARK_OAK_PLANKS = count++;
export const ACACIA_PLANKS = count++;
export const CHERRY_PLANKS = count++;
export const CACTUS = count++;
/** From here to `LAMP`, foliage: it thins the sky's light and darkens no corner of the ground. */
export const FOLIAGE = count;
export const OAK_LEAVES = count++;
export const BIRCH_LEAVES = count++;
export const SPRUCE_LEAVES = count++;
/** Spruce foliage with snow lying on it. */
export const SNOWY_LEAVES = count++;
export const DARK_OAK_LEAVES = count++;
export const ACACIA_LEAVES = count++;
export const CHERRY_LEAVES = count++;
/** From here to `WATER`, what gives light, each in its own colour: the lamp of a lighthouse. */
export const LAMP = count++;
/** A window with a fire lit behind it. */
export const WINDOW = count++;
export const LAVA = count++;
/** What glows from the bed of the water: magma in the deep, sea pickles in warm shallows. */
export const MAGMA = count++;
export const SEA_PICKLES = count++;
export const WATER = count++;
export const MATERIALS = count;

/** Paint per material for its top, side and underside. */
export const FACES = new Uint8Array(MATERIALS * 3);
/** Paint per ground material of the soil just under its surface block, and of the rock under that. */
export const BELOW = new Uint8Array(MATERIALS * 2);
const faces = (mat: number, top: number, side = top, under = side): void => FACES.set([top, side, under], mat * 3);
const below = (mat: number, soil: number, rock: number = T.STONE_TEX): void => BELOW.set([soil, rock], mat * 2);
faces(GRASS, T.GRASS_TOP, T.GRASS_SIDE, T.DIRT_TEX);
faces(DIRT, T.DIRT_TEX);
faces(PODZOL, T.PODZOL_TOP, T.PODZOL_SIDE, T.DIRT_TEX);
faces(STONE, T.STONE_TEX);
faces(GRAVEL, T.GRAVEL_TEX);
faces(SAND, T.SAND_TEX);
faces(SANDSTONE, T.SANDSTONE_TEX);
faces(RED_SAND, T.RED_SAND_TEX);
faces(SNOWY_GRASS, T.SNOW_TEX, T.SNOW_SIDE, T.DIRT_TEX);
faces(SNOW, T.SNOW_TEX);
faces(ICE, T.ICE_TEX);
faces(TERRACOTTA, T.CLAY_PLAIN);
faces(TILES, T.CLAY_BROWN);
faces(PLASTER, T.CLAY_WHITE);
faces(THATCH, T.CLAY_YELLOW);
faces(OAK_LOG, T.OAK_TOP, T.OAK_SIDE, T.OAK_TOP);
faces(BIRCH_LOG, T.BIRCH_TOP, T.BIRCH_SIDE, T.BIRCH_TOP);
faces(SPRUCE_LOG, T.SPRUCE_TOP, T.SPRUCE_SIDE, T.SPRUCE_TOP);
faces(DARK_OAK_LOG, T.DARK_OAK_TOP, T.DARK_OAK_SIDE, T.DARK_OAK_TOP);
faces(ACACIA_LOG, T.ACACIA_TOP, T.ACACIA_SIDE, T.ACACIA_TOP);
faces(CHERRY_LOG, T.CHERRY_TOP, T.CHERRY_SIDE, T.CHERRY_TOP);
// Sawn wood is the colour of a log's cut end.
faces(OAK_PLANKS, T.OAK_TOP);
faces(BIRCH_PLANKS, T.BIRCH_TOP);
faces(SPRUCE_PLANKS, T.SPRUCE_TOP);
faces(DARK_OAK_PLANKS, T.DARK_OAK_TOP);
faces(ACACIA_PLANKS, T.ACACIA_TOP);
faces(CHERRY_PLANKS, T.CHERRY_TOP);
faces(CACTUS, T.CACTUS_TOP, T.CACTUS_SIDE);
faces(OAK_LEAVES, T.OAK_LEAVES_TEX);
faces(BIRCH_LEAVES, T.BIRCH_LEAVES_TEX);
faces(SPRUCE_LEAVES, T.SPRUCE_LEAVES_TEX);
faces(SNOWY_LEAVES, T.SNOW_TEX, T.SPRUCE_LEAVES_TEX);
faces(DARK_OAK_LEAVES, T.DARK_OAK_LEAVES_TEX);
faces(ACACIA_LEAVES, T.ACACIA_LEAVES_TEX);
faces(CHERRY_LEAVES, T.CHERRY_LEAVES_TEX);
for (let mat = 1; mat < MATERIALS; mat++) below(mat, FACES[mat * 3 + 1]!, FACES[mat * 3 + 1]);
for (const mat of [GRASS, DIRT, PODZOL, SNOWY_GRASS]) below(mat, T.DIRT_TEX);
below(GRAVEL, T.GRAVEL_TEX);
below(SAND, T.SAND_TEX, T.SANDSTONE_TEX);
// Under red sand lies the banded clay; the renderer reads plain clay as "the band of this level".
below(RED_SAND, T.RED_SAND_TEX, T.CLAY_PLAIN);

/** How brightly each material that gives light shows itself; 0 for the rest. */
export const GLOWS = new Float32Array(MATERIALS);
GLOWS[LAMP] = 1;
GLOWS[WINDOW] = 0.5;
GLOWS[LAVA] = 0.8;
GLOWS[MAGMA] = 0.5;
GLOWS[SEA_PICKLES] = 0.6;

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
