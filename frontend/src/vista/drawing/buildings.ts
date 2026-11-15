/**
 * What stands by the water in a study: each a few rooms of blocks with a fire lit inside, set down by hand.
 */
import * as B from './blocks';

const { PLASTER, THATCH, TILES, WINDOW, LAMP } = B;
/** Stonework, and the slates of a stone roof. */
const STONE = B.STONE;
const SAIL = B.BIRCH_LOG;
const DOOR = B.DARK_OAK_PLANKS;

/**
 * Where a building is raised: blocks go by its own near left corner, across, away and up from its yard. A house
 * takes its walls, its roof and its timber from the biome it stands in.
 */
interface Site {
  walls: number;
  roof: number;
  timber: number;
  put(x: number, y: number, z: number, mat: number, tall?: number): void;
  /** A block that gives light, and the light it gives. */
  lamp(x: number, y: number, z: number): void;
}

export interface Building {
  /** The ground it needs, and where across its front the way in is. */
  wide: number;
  deep: number;
  door: number;
  /**
   * Where its houses stand within that ground, each by its near left corner and its size, when they are several
   * with open ground between them.
   */
  plots?: readonly (readonly [x: number, y: number, wide: number, deep: number])[];
  raise(site: Site): void;
}

/** A site `dx` across and `dy` away from another. */
const beside = (site: Site, dx: number, dy: number): Site => ({
  ...site,
  put: (x, y, z, mat, tall) => site.put(x + dx, y + dy, z, mat, tall),
  lamp: (x, y, z) => site.lamp(x + dx, y + dy, z),
});

/** An opening in a wall: from what height to what, and what fills it. */
type Opening = readonly [from: number, to: number, mat: number];

/**
 * The four walls of a room `w` by `d` and `h` high, their corners of another material, with whatever openings
 * `open` names for a place along them, lowest first.
 */
function walls(
  site: Site,
  x0: number,
  y0: number,
  w: number,
  d: number,
  h: number,
  wall: number,
  corner: number,
  open: (x: number, y: number) => readonly Opening[],
  z0 = 0,
): void {
  for (let x = x0; x < x0 + w; x++) {
    for (let y = y0; y < y0 + d; y++) {
      const edgeX = x === x0 || x === x0 + w - 1;
      const edgeY = y === y0 || y === y0 + d - 1;
      if (!edgeX && !edgeY) continue;
      const of = edgeX && edgeY ? corner : wall;
      let from = 0;
      for (const [low, high, mat] of open(x - x0, y - y0)) {
        site.put(x, y, z0 + from, of, low - from);
        site.put(x, y, z0 + low, mat, high - low);
        from = high;
      }
      site.put(x, y, z0 + from, of, h - from);
    }
  }
}

/**
 * A pitched roof over a room, a block of eave all round. Its ridge runs across the picture, so the slope faces
 * it, or away, so a gable does; the gables are closed with `gable`.
 */
function pitched(
  site: Site,
  x0: number,
  y0: number,
  w: number,
  d: number,
  z: number,
  mat: number,
  gable: number,
  away = false,
): void {
  for (let x = x0 - 1; x <= x0 + w; x++) {
    for (let y = y0 - 1; y <= y0 + d; y++) {
      const step = away ? Math.min(x - (x0 - 1), x0 + w - x) : Math.min(y - (y0 - 1), y0 + d - y);
      const end = away ? y === y0 || y === y0 + d - 1 : x === x0 || x === x0 + w - 1;
      const inside = x >= x0 && x < x0 + w && y >= y0 && y < y0 + d;
      if (end && inside) site.put(x, y, z, gable, step);
      site.put(x, y, z + step, mat);
    }
  }
}

/**
 * A roof drawn in to a point over a square room `w` a side, each ring of it `steep` blocks high, with a finial
 * `point` blocks tall: low for a cap, steep for a spire.
 */
function pointed(site: Site, x0: number, y0: number, w: number, z: number, mat: number, point = 0, steep = 1): void {
  const half = (w - 1) / 2;
  for (let x = x0; x < x0 + w; x++)
    for (let y = y0; y < y0 + w; y++)
      site.put(x, y, z + (half - Math.max(Math.abs(x - x0 - half), Math.abs(y - y0 - half))) * steep, mat, steep);
  site.put(x0 + half, y0 + half, z + (half + 1) * steep, mat, point);
}

const NONE: readonly Opening[] = [];
/** A window lit from within, two blocks high, a block off the floor. */
const LIT: readonly Opening[] = [[1, 3, WINDOW]];
const WAY_IN: readonly Opening[] = [[0, 2, DOOR]];

/**
 * A house of one room: its walls and a roof, its door, and a pair of lit windows from each place in
 * `lit` along its front.
 */
function house(
  site: Site,
  w: number,
  d: number,
  h: number,
  wall: number,
  roof: number,
  away: boolean,
  door: number,
  lit: readonly number[],
): void {
  walls(site, 0, 0, w, d, h, wall, site.timber, (x, y) => {
    if (y !== 0) return NONE;
    return x === door ? WAY_IN : lit.some((from) => x === from || x === from + 1) ? LIT : NONE;
  });
  pitched(site, 0, 0, w, d, h, roof, wall, away);
}

/** A cabin of the biome's own wood. */
const cabin: Building = {
  wide: 11,
  deep: 8,
  door: 5,
  raise: (site) => house(site, 11, 8, 4, site.walls, site.roof, false, 5, [2, 7]),
};

/** A plastered cottage, its thatched gable to the water. */
const cottage: Building = {
  wide: 9,
  deep: 8,
  door: 4,
  raise: (site) => house(site, 9, 8, 4, PLASTER, THATCH, true, 4, [1, 6]),
};

/** A long barn on a stone footing, its great doors open on the light inside. */
const barn: Building = {
  wide: 13,
  deep: 7,
  door: 6,
  raise(site) {
    const doors = (x: number, y: number): boolean => y === 0 && Math.abs(x - 6) < 2;
    walls(site, 0, 0, 13, 7, 1, STONE, STONE, (x, y) => (doors(x, y) ? [[0, 1, WINDOW]] : NONE));
    walls(
      site,
      0,
      0,
      13,
      7,
      4,
      site.walls,
      site.timber,
      (x, y) => {
        if (doors(x, y)) return [[0, 2, WINDOW]];
        return y === 0 && (x === 1 || x === 2 || x === 10 || x === 11) ? [[0, 2, WINDOW]] : NONE;
      },
      1,
    );
    pitched(site, 0, 0, 13, 7, 5, site.roof, site.walls);
  },
};

/** A stone keep with its battlements, and a hall built against it. */
const keep: Building = {
  wide: 12,
  deep: 7,
  door: 8,
  raise(site) {
    walls(site, 6, 1, 5, 5, 12, STONE, STONE, (x, y) => {
      if (y === 0 && x === 2)
        return [
          [3, 5, WINDOW],
          [8, 10, WINDOW],
        ];
      return x === 4 && y === 2 ? [[8, 10, WINDOW]] : NONE;
    });
    for (let x = 6; x < 11; x++)
      for (let y = 1; y < 6; y++) {
        const rim = x === 6 || x === 10 || y === 1 || y === 5;
        site.put(x, y, 11, STONE, rim && (x + y) % 2 === 1 ? 2 : 1);
      }
    walls(site, 0, 1, 6, 5, 3, STONE, STONE, (x, y) => (y === 0 && (x === 2 || x === 3) ? LIT : NONE));
    pitched(site, 0, 1, 6, 5, 3, site.roof, STONE);
  },
};

/** A windmill: a stone foot with its door open on the light, a plastered tower under a cap, and four sails. */
const windmill: Building = {
  wide: 9,
  deep: 7,
  door: 4,
  raise(site) {
    walls(site, 2, 1, 5, 5, 4, STONE, STONE, (x, y) => (y === 0 && x === 2 ? [[0, 2, WINDOW]] : NONE));
    walls(site, 2, 1, 5, 5, 6, PLASTER, PLASTER, (x, y) => (y === 0 && x === 3 ? LIT : NONE), 4);
    pointed(site, 1, 0, 7, 10, STONE);
    // The sails turn on a hub before the tower, clear of its door: each a spar with its cloth to one side.
    site.put(4, -1, 9, site.timber);
    for (let out = 1; out <= 5; out++) {
      site.put(4, -1, 9 + out, site.timber);
      site.put(4, -1, 9 - out, site.timber);
      site.put(4 + out, -1, 9, site.timber);
      site.put(4 - out, -1, 9, site.timber);
      if (out < 2) continue;
      site.put(5, -1, 9 + out, SAIL);
      site.put(3, -1, 9 - out, SAIL);
      site.put(4 + out, -1, 8, SAIL);
      site.put(4 - out, -1, 10, SAIL);
    }
  },
};

/** A chapel: a bell tower under a spire, and a nave with tall lit windows. */
const chapel: Building = {
  wide: 13,
  deep: 7,
  door: 2,
  raise(site) {
    walls(site, 0, 0, 5, 5, 10, STONE, STONE, (x, y) => {
      if (y === 0 && x === 2) return [...WAY_IN, [6, 8, WINDOW]];
      return x === 4 && y === 2 ? [[6, 8, WINDOW]] : NONE;
    });
    pointed(site, 0, 0, 5, 10, site.roof, 1, 2);
    walls(site, 5, 0, 8, 7, 5, PLASTER, STONE, (x, y) => (y === 0 && x % 2 === 1 && x < 7 ? [[1, 4, WINDOW]] : NONE));
    pitched(site, 5, 0, 8, 7, 5, site.roof, PLASTER);
  },
};

/** A lighthouse: a banded tower, a gallery, the lamp room and its cap; the keeper's house beside it. */
const lighthouse: Building = {
  wide: 12,
  deep: 6,
  door: 2,
  raise(site) {
    for (let x = 0; x < 5; x++) {
      for (let y = 0; y < 5; y++) {
        const edgeX = x === 0 || x === 4;
        const edgeY = y === 0 || y === 4;
        if (edgeX && edgeY) continue;
        if (edgeX || edgeY) {
          const front = y === 0 && x === 2;
          site.put(x, y, 0, front ? DOOR : PLASTER, 2);
          site.put(x, y, 2, PLASTER, 3);
          site.put(x, y, 5, TILES, 2);
          site.put(x, y, 7, PLASTER, front ? 1 : 3);
          if (front) site.put(x, y, 8, WINDOW, 2);
        }
      }
    }
    for (let x = -1; x < 6; x++) for (let y = -1; y < 6; y++) site.put(x, y, 10, STONE);
    for (let x = 1; x < 4; x++) for (let y = 1; y < 4; y++) site.put(x, y, 11, LAMP, 2);
    pointed(site, 0, 0, 5, 13, STONE, 1);
    site.lamp(2, 1, 12);
    house(beside(site, 6, 1), 6, 5, 3, site.walls, site.roof, false, 1, [3]);
  },
};

/**
 * A village: three houses of different makes, each on its own ground: one by the water, one back to its left and
 * one further back to its right.
 */
const village: Building = {
  wide: 40,
  deep: 14,
  door: 17,
  plots: [
    [0, 3, 7, 6],
    [13, 0, 9, 7],
    [33, 8, 7, 6],
  ],
  raise(site) {
    house(beside(site, 0, 3), 7, 6, 3, site.walls, site.roof, true, 1, [4]);
    house(beside(site, 13, 0), 9, 7, 4, PLASTER, site.roof, false, 4, [1, 6]);
    house(beside(site, 33, 8), 7, 6, 4, STONE, site.roof, true, 5, [1]);
  },
};

/** How high a building stands above its yard, in blocks. */
export function tallness(building: Building): number {
  let top = 0;
  const none = (): void => undefined;
  building.raise({
    walls: 0,
    roof: 0,
    timber: 0,
    lamp: none,
    put: (_x, _y, z, _mat, tall = 1) => void (top = Math.max(top, z + tall)),
  });
  return top;
}

/** The buildings there are, by name. */
export const BUILDINGS = { cabin, cottage, barn, keep, windmill, chapel, lighthouse, village } satisfies Record<
  string,
  Building
>;
