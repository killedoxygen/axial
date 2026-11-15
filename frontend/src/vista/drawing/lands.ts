/**
 * The lies of the land a picture may have, each set down by hand: where its water is, where a building stands by
 * it, how the country rises round it, and where it is seen from. Places go across, away from where the picture is
 * seen from, and up from the water.
 */

/** The kinds of lie there are: a biome is found in some and not in others. */
export type LieName = 'shore' | 'river' | 'isles' | 'fjord' | 'range' | 'plains' | 'cliffs';

export interface Land {
  seed: number;
  /** The kind of lie it is: a biome is found in some kinds and not in others. */
  lie: LieName;
  /** Where it is seen from, and a point it is looked toward. */
  camera: { x: number; y: number };
  look: { x: number; y: number };
  /** The water's middle and reach away, and how many times further it reaches across. */
  water: { x: number; y: number; r: number; wide: number };
  /** Where a building ten blocks wide has its near left corner, and how high above the water its yard lies. */
  site: { x: number; y: number; rise: number };
  /** How strongly the land rises round the water, far off, and to either side, against the first land's. */
  hills: number;
  far: number;
  sides: number;
  /** Whether it rises to the right alone, the left lying open to the horizon. */
  open?: boolean;
  /** Trees near enough to frame the view: where, and how tall. */
  frame: readonly (readonly [number, number, number])[];
}

export const LANDS = {
  // A lake, the building on its far shore.
  lake: {
    seed: 7,
    lie: 'shore',
    camera: { x: -9, y: -34 },
    look: { x: 13, y: 36 },
    water: { x: 6, y: 22, r: 24, wide: 1.55 },
    site: { x: 22, y: 46, rise: 2 },
    hills: 1,
    far: 1,
    sides: 1,
    frame: [
      [-16, -6, 11],
      [12, -9, 10],
      [18, -5, 11],
    ],
  },
  // A valley, the building on a rise above a narrower pond.
  pond: {
    seed: 23,
    lie: 'shore',
    camera: { x: 8, y: -33 },
    look: { x: -31, y: 36 },
    water: { x: -4, y: 20, r: 17, wide: 1.55 },
    site: { x: -22, y: 44, rise: 2 },
    hills: 1,
    far: 1,
    sides: 1,
    frame: [[-9, -22, 11]],
  },
  // A small tarn under high ground, the building close across it.
  tarn: {
    seed: 41,
    lie: 'fjord',
    camera: { x: -4, y: -34 },
    look: { x: 10, y: 34 },
    water: { x: 4, y: 12, r: 13, wide: 1.4 },
    site: { x: 12, y: 30, rise: 2 },
    hills: 1.3,
    far: 1.2,
    sides: 1.2,
    frame: [[-13, -10, 11]],
  },
  // A broad reach of water, the building small on the far side.
  reach: {
    seed: 58,
    lie: 'shore',
    camera: { x: 0, y: -34 },
    look: { x: 16, y: 44 },
    water: { x: 2, y: 30, r: 32, wide: 2.4 },
    site: { x: 22, y: 64, rise: 2 },
    hills: 0.8,
    far: 0.9,
    sides: 0.5,
    frame: [[20, -8, 11]],
  },
  // The building on a knoll above the water.
  knoll: {
    seed: 77,
    lie: 'cliffs',
    camera: { x: -6, y: -34 },
    look: { x: 14, y: 36 },
    water: { x: 2, y: 18, r: 20, wide: 1.55 },
    site: { x: 18, y: 44, rise: 7 },
    hills: 1.1,
    far: 1.2,
    sides: 1,
    frame: [[-14, -8, 11]],
  },
  // A river running across, the building just over it.
  narrows: {
    seed: 93,
    lie: 'river',
    camera: { x: -2, y: -34 },
    look: { x: 12, y: 30 },
    water: { x: 0, y: 8, r: 9, wide: 9 },
    site: { x: 12, y: 24, rise: 2 },
    hills: 0.5,
    far: 0.75,
    sides: 0.3,
    frame: [[-12, -12, 11]],
  },
  // The building on an island of its own, the far shore behind it.
  island: {
    seed: 112,
    lie: 'isles',
    camera: { x: -8, y: -34 },
    look: { x: 12, y: 36 },
    water: { x: 6, y: 30, r: 30, wide: 1.7 },
    site: { x: 16, y: 30, rise: 2 },
    hills: 1,
    far: 1,
    sides: 0.9,
    frame: [[-15, -7, 11]],
  },
  // Low open country under a wide sky, the building across a mere.
  plain: {
    seed: 131,
    lie: 'plains',
    camera: { x: -5, y: -34 },
    look: { x: 14, y: 36 },
    water: { x: 4, y: 20, r: 20, wide: 1.9 },
    site: { x: 20, y: 42, rise: 2 },
    hills: 0.45,
    far: 0.5,
    sides: 0.4,
    frame: [[16, -8, 10]],
  },
  // High mountains behind the water, snow on their heads.
  peaks: {
    seed: 152,
    lie: 'range',
    camera: { x: -7, y: -34 },
    look: { x: 13, y: 36 },
    water: { x: 6, y: 20, r: 22, wide: 1.6 },
    site: { x: 20, y: 43, rise: 2 },
    hills: 1.3,
    far: 2.5,
    sides: 1.5,
    frame: [[-15, -7, 11]],
  },
  // The sea, open to the horizon, and the building on its shore.
  coast: {
    seed: 171,
    lie: 'shore',
    camera: { x: -9, y: -34 },
    look: { x: 13, y: 36 },
    water: { x: -40, y: 110, r: 120, wide: 3 },
    site: { x: 24, y: 38, rise: 2 },
    hills: 1,
    far: 0,
    sides: 1.2,
    open: true,
    frame: [[14, -9, 10]],
  },
} satisfies Record<string, Land>;

export type LandName = keyof typeof LANDS;
