/**
 * The countries a picture may lie in, as the game has them: what the ground is made of, what grows on it, what a
 * house is built of there, and the light of its evening.
 */
import * as B from './blocks';
import type { LieName } from './lands';
import type { Tree } from './trees';
import type { Dusk } from './world';

export interface Biome {
  name: string;
  /** The colours of its evening. */
  dusk: Dusk;
  surface: number;
  /** The ground at the water's edge, and under the water. */
  shore: number;
  bed: number;
  trees: [kind: Tree, share: number][];
  /** The lies of the land it is found in. */
  lies: LieName[];
  density: number;
  /** Rolling of the land, and how high it stands at the back of the picture, in blocks. */
  relief: number;
  rise: number;
  /** The biome's grass and foliage colours (sRGB), which tint the grey textures. */
  grass: number;
  foliage: number;
  /** What a house is built of here: its walls, and its roof. */
  walls: number;
  roof: number;
  frozen?: boolean;
  /** High ground turns to bare stone, then snow. */
  alpine?: boolean;
  /** The land rises in flat-topped steps of banded clay. */
  mesa?: boolean;
  /** Its trees stand out in the shallows too. */
  flooded?: boolean;
  /** A second surface in patches, as leaf litter lies under old spruce. */
  patches?: number;
}

/** The game's biomes: what each is made of, what grows in it and how thickly, and how the land lies. */
export const BIOMES: Biome[] = [
  // Plains.
  {
    name: 'plains',
    dusk: { glow: [1.0, 0.55, 0.22], cool: [0.08, 0.2, 0.16], fill: [0.16, 0.48, 0.3], haze: [0.06, 0.085, 0.07] },
    surface: B.GRASS,
    shore: B.SAND,
    bed: B.SAND,
    trees: [
      ['oak', 5],
      ['tall oak', 1],
    ],
    lies: ['shore', 'river', 'plains', 'isles', 'cliffs'],
    density: 0.1,
    relief: 4,
    rise: 8,
    grass: 0x91bd59,
    foliage: 0x77ab2f,
    walls: B.OAK_PLANKS,
    roof: B.OAK_LOG,
  },
  // Forest.
  {
    name: 'forest',
    dusk: { glow: [0.95, 0.62, 0.26], cool: [0.08, 0.22, 0.17], fill: [0.14, 0.5, 0.3], haze: [0.055, 0.085, 0.075] },
    surface: B.GRASS,
    shore: B.SAND,
    bed: B.SAND,
    trees: [
      ['oak', 4],
      ['birch', 2],
      ['tall oak', 0.4],
    ],
    lies: ['shore', 'river', 'range', 'fjord', 'isles'],
    density: 0.5,
    relief: 6,
    rise: 18,
    grass: 0x79c05a,
    foliage: 0x59ae30,
    walls: B.OAK_PLANKS,
    roof: B.OAK_LOG,
    alpine: true,
  },
  // Birch forest.
  {
    name: 'birch',
    dusk: { glow: [1.0, 0.6, 0.28], cool: [0.1, 0.2, 0.14], fill: [0.24, 0.5, 0.26], haze: [0.07, 0.09, 0.07] },
    surface: B.GRASS,
    shore: B.SAND,
    bed: B.GRAVEL,
    trees: [['birch', 1]],
    lies: ['shore', 'river', 'plains', 'isles'],
    density: 0.42,
    relief: 5,
    rise: 12,
    grass: 0x88bb67,
    foliage: 0x6ba941,
    walls: B.BIRCH_PLANKS,
    roof: B.STONE,
  },
  // Dark forest.
  {
    name: 'dark',
    dusk: { glow: [0.95, 0.4, 0.2], cool: [0.05, 0.14, 0.14], fill: [0.08, 0.36, 0.34], haze: [0.045, 0.065, 0.07] },
    surface: B.GRASS,
    shore: B.GRASS,
    bed: B.GRAVEL,
    trees: [
      ['dark oak', 8],
      ['oak', 1.5],
      ['birch', 0.5],
    ],
    lies: ['shore', 'river', 'isles'],
    density: 0.6,
    relief: 4,
    rise: 10,
    grass: 0x507a32,
    foliage: 0x59ae30,
    walls: B.DARK_OAK_PLANKS,
    roof: B.DARK_OAK_LOG,
  },
  // Taiga.
  {
    name: 'taiga',
    dusk: { glow: [1.0, 0.44, 0.17], cool: [0.06, 0.16, 0.2], fill: [0.1, 0.42, 0.52], haze: [0.07, 0.065, 0.1] },
    surface: B.GRASS,
    shore: B.GRASS,
    bed: B.GRAVEL,
    trees: [['spruce', 1]],
    lies: ['shore', 'river', 'range', 'fjord', 'cliffs'],
    density: 0.55,
    relief: 7,
    rise: 26,
    grass: 0x86b783,
    foliage: 0x68a464,
    walls: B.SPRUCE_PLANKS,
    roof: B.SPRUCE_LOG,
    alpine: true,
    patches: B.PODZOL,
  },
  // Snowy taiga, its water frozen.
  {
    name: 'snowy',
    dusk: { glow: [1.0, 0.52, 0.34], cool: [0.1, 0.16, 0.26], fill: [0.22, 0.38, 0.6], haze: [0.085, 0.095, 0.135] },
    surface: B.SNOWY_GRASS,
    shore: B.SNOWY_GRASS,
    bed: B.DIRT,
    trees: [['spruce', 1]],
    lies: ['shore', 'range', 'fjord', 'plains'],
    density: 0.36,
    relief: 6,
    rise: 28,
    grass: 0x80b497,
    foliage: 0x60a17b,
    walls: B.SPRUCE_PLANKS,
    roof: B.SNOW,
    frozen: true,
    alpine: true,
  },
  // Savanna.
  {
    name: 'savanna',
    dusk: { glow: [1.0, 0.5, 0.14], cool: [0.14, 0.13, 0.1], fill: [0.42, 0.37, 0.2], haze: [0.09, 0.07, 0.056] },
    surface: B.GRASS,
    shore: B.SAND,
    bed: B.SAND,
    trees: [
      ['acacia', 6],
      ['oak', 1],
    ],
    lies: ['shore', 'river', 'plains', 'cliffs'],
    density: 0.16,
    relief: 5,
    rise: 12,
    grass: 0xbfb755,
    foliage: 0xaea42a,
    walls: B.ACACIA_PLANKS,
    roof: B.ACACIA_LOG,
  },
  // Desert.
  {
    name: 'desert',
    dusk: { glow: [1.0, 0.52, 0.2], cool: [0.16, 0.13, 0.13], fill: [0.55, 0.42, 0.32], haze: [0.11, 0.078, 0.07] },
    surface: B.SAND,
    shore: B.SAND,
    bed: B.SAND,
    trees: [['cactus', 1]],
    lies: ['shore', 'river', 'plains', 'isles'],
    density: 0.07,
    relief: 6,
    rise: 10,
    grass: 0xbfb755,
    foliage: 0xaea42a,
    walls: B.SANDSTONE,
    roof: B.TILES,
  },
  // Badlands.
  {
    name: 'badlands',
    dusk: { glow: [1.0, 0.36, 0.14], cool: [0.14, 0.1, 0.13], fill: [0.42, 0.3, 0.34], haze: [0.1, 0.062, 0.07] },
    surface: B.RED_SAND,
    shore: B.RED_SAND,
    bed: B.RED_SAND,
    trees: [['cactus', 1]],
    lies: ['river', 'shore', 'fjord'],
    density: 0.04,
    relief: 4,
    rise: 34,
    grass: 0x90814d,
    foliage: 0x9e814d,
    walls: B.SANDSTONE,
    roof: B.TILES,
    mesa: true,
  },
  // Swamp.
  {
    name: 'swamp',
    dusk: { glow: [0.9, 0.5, 0.25], cool: [0.12, 0.2, 0.15], fill: [0.34, 0.52, 0.34], haze: [0.058, 0.072, 0.055] },
    surface: B.GRASS,
    shore: B.GRASS,
    bed: B.DIRT,
    trees: [['swamp oak', 1]],
    lies: ['shore', 'isles', 'plains'],
    density: 0.2,
    relief: 2,
    rise: 3,
    grass: 0x6a7039,
    foliage: 0x6a7039,
    walls: B.OAK_PLANKS,
    roof: B.OAK_LOG,
    flooded: true,
  },
  // Cherry grove.
  {
    name: 'cherry',
    dusk: { glow: [1.0, 0.42, 0.3], cool: [0.1, 0.13, 0.2], fill: [0.42, 0.36, 0.44], haze: [0.09, 0.066, 0.082] },
    surface: B.GRASS,
    shore: B.GRASS,
    bed: B.GRAVEL,
    trees: [
      ['cherry', 6],
      ['birch', 1],
    ],
    lies: ['shore', 'range', 'river', 'isles'],
    density: 0.36,
    relief: 6,
    rise: 30,
    grass: 0xb6db61,
    foliage: 0xb6db61,
    walls: B.CHERRY_PLANKS,
    roof: B.CHERRY_LOG,
    alpine: true,
  },
  // Meadow under mountains.
  {
    name: 'meadow',
    dusk: { glow: [1.0, 0.5, 0.24], cool: [0.07, 0.18, 0.22], fill: [0.14, 0.44, 0.46], haze: [0.065, 0.075, 0.095] },
    surface: B.GRASS,
    shore: B.GRASS,
    bed: B.GRAVEL,
    trees: [
      ['oak', 2],
      ['birch', 1],
    ],
    lies: ['range', 'shore', 'fjord'],
    density: 0.05,
    relief: 5,
    rise: 40,
    grass: 0x83bb6d,
    foliage: 0x63a948,
    walls: B.OAK_PLANKS,
    roof: B.STONE,
    alpine: true,
  },
];
