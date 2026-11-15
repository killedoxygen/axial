import { store, stored, storedWidth } from './store';
import { ASPECT, SHALLOW } from './drawing/strips';
import { draw, packed, unpacked } from './studio';

/** The most pixels a picture is drawn with: a whole one as wide as a large window on a dense display. */
const PIXELS = 7_000_000;
const WIDEST = Math.floor(Math.sqrt(PIXELS * ASPECT));
/** A place this wide or narrower, in device pixels, gets a picture of its own size: a large one shrinks badly. */
const SMALL = 960;
/** What the kept pictures may hold between them in memory: three of the largest. */
const KEPT_BYTES = PIXELS * 4 * 3;
/** How many seeds have their pictures drawn ahead of any place asking. */
const AHEAD = 12;
/** What the store may hold, packed: those seeds' pictures at the largest size several times over. */
const STORED_BYTES = 128 * 1024 * 1024;
/** How long after the seeds to draw ahead were last named the drawing begins. */
const AHEAD_MS = 1500;
/** How long a place's size must hold still before its picture is drawn for it. */
const SETTLE_MS = 400;

/**
 * One whole picture is kept for a seed and every place near its shape shares it: Home, and the same place at
 * another window size. Each scales and crops it in its stylesheet, so a change of page or of
 * size shows a picture at once. It is drawn as wide as the place that first asks, and again only for a place so
 * much wider that scaling would soften it.
 *
 * A place much shallower than a whole picture, as the head of an instance's page is, has no good crop of it: it
 * has a picture of its own shape, with the building whole in it.
 *
 * A picture is stored as it is drawn, and one not kept is read from there before it is drawn. The seeds named to
 * `warm` are drawn and stored ahead, so a place seldom waits for a drawing at all.
 */
const kept = new Map<string, ImageData>();
/**
 * How wide the last large, the last small and the last shallow place asked for their pictures, in device pixels,
 * and the last shallow place's shape: what is drawn ahead.
 */
const asked = { large: 0, small: 0, shallow: 0, shape: 0 };

/** A picture to have, and how large. */
interface Order {
  key: string;
  width: number;
  height: number;
  /** Set once whoever asked no longer wants it. */
  cancelled: boolean;
  /** Drawn before any place asks: every place's order goes first, and the picture is stored without being kept. */
  ahead?: boolean;
}

/** An order for a whole picture `width` across. */
const whole = (key: string, width: number, ahead?: boolean): Order => ({
  key,
  width,
  height: Math.round(width / ASPECT),
  cancelled: false,
  ahead,
});

/**
 * An order for a shallow place's own picture `width` across. Shapes go by quarters, so a place resized a little
 * keeps its picture.
 */
const shallow = (key: string, width: number, shape: number, ahead?: boolean): Order => ({
  key: `${key}:${shape}`,
  width,
  height: Math.round(width / shape),
  cancelled: false,
  ahead,
});

/** How wide a picture is drawn for a place `wanted` device pixels across. */
const drawnFor = (wanted: number): number => Math.min(WIDEST, Math.max(320, Math.ceil(wanted / 64) * 64));

/** Whether `picture` is sharp at `width`. */
const serves = (picture: ImageData | undefined, width: number): picture is ImageData =>
  !!picture && picture.width >= width * 0.8;

function keep(key: string, picture: ImageData): void {
  kept.delete(key);
  kept.set(key, picture);
  let bytes = 0;
  for (const each of kept.values()) bytes += each.data.length;
  for (const [oldest, each] of kept) {
    if (bytes <= KEPT_BYTES || oldest === key) break;
    bytes -= each.data.length;
    kept.delete(oldest);
  }
}

/**
 * Orders take turns, the widest first and those drawn ahead last: each drawing has every worker, and the largest
 * place is the one looked at. The first of a burst waits out the rest of its task, so that whichever asked first,
 * the widest begins; and an order in hand gives way to one that outranks it, then takes another turn.
 */
const waiting: { order: Order; begin: () => void }[] = [];
let busy = false;
let inHand: Order | null = null;
let outranked = false;

const rank = (order: Order): number => (order.ahead ? 0 : order.width);

function proceed(): void {
  let first = 0;
  for (let i = 1; i < waiting.length; i++) {
    if (rank(waiting[i]!.order) > rank(waiting[first]!.order)) first = i;
  }
  const [turn] = waiting.splice(first, 1);
  busy = !!turn;
  inHand = turn?.order ?? null;
  outranked = false;
  turn?.begin();
}

function turn(order: Order): Promise<void> {
  return new Promise((begin) => {
    waiting.push({ order, begin });
    if (inHand && rank(order) > rank(inHand)) outranked = true;
    if (busy) return;
    busy = true;
    queueMicrotask(proceed);
  });
}

/** The picture an order asks for, in its turn: the one kept, else the one stored, else drawn and stored. */
async function obtain(seed: number, order: Order): Promise<ImageData | undefined> {
  for (;;) {
    await turn(order);
    try {
      if (order.cancelled) return undefined;
      let picture = kept.get(order.key);
      if (serves(picture, order.width)) return picture;
      const file = await stored(order.key);
      picture = file && (await unpacked(file));
      if (!serves(picture, order.width)) {
        const stopped = (): boolean => order.cancelled || outranked;
        picture = (await draw(stopped, seed, order.width, order.height)) ?? undefined;
        if (!picture && outranked && !order.cancelled) continue;
        const drawn = picture;
        if (drawn) void packed(drawn).then((file) => file && store(order.key, file, drawn.width, STORED_BYTES));
      }
      if (picture && !order.ahead) keep(order.key, picture);
      return picture;
    } finally {
      proceed();
    }
  }
}

/**
 * Shows a seed's picture on `canvas` and keeps it right for the canvas's size until the returned function is called. A kept
 * picture is on the canvas before this returns; a softer one stands in while a sharper one is fetched.
 */
export function show(canvas: HTMLCanvasElement, seed: number): () => void {
  const large = String(seed);
  let order: Order | null = null;
  let shown: ImageData | undefined;
  let settle = 0;
  const put = (picture: ImageData, ready: string): void => {
    if (shown !== picture) {
      shown = picture;
      canvas.width = picture.width;
      canvas.height = picture.height;
      // Kept where the page can read it: it is painted with the page each time its part of the page is drawn
      // again, and a canvas held for drawing costs a frame every time.
      canvas.getContext('2d', { willReadFrequently: true })?.putImageData(picture, 0, 0);
    }
    canvas.dataset.ready ??= ready;
  };
  const paint = (): void => {
    const box = canvas.getBoundingClientRect();
    const ratio = window.devicePixelRatio || 1;
    let next: Order;
    let have: ImageData | undefined;
    let sharp: number;
    if (box.width > box.height * ASPECT * SHALLOW) {
      // A shallow place has no good crop of the whole picture: it has one of its own shape.
      const shape = Math.round((box.width / box.height) * 4) / 4;
      const wide = Math.min(Math.ceil((box.width * ratio) / 64), Math.floor(Math.sqrt(PIXELS * shape) / 64)) * 64;
      if (!(wide >= 64)) return;
      asked.shallow = wide;
      asked.shape = shape;
      next = shallow(large, wide, shape);
      sharp = wide;
      have = kept.get(next.key);
    } else {
      // How wide the whole picture stands here: a place narrower than the picture crops its sides.
      const wanted = Math.max(box.width, box.height * ASPECT) * ratio;
      if (!(wanted >= 16)) return;
      asked[wanted > SMALL ? 'large' : 'small'] = wanted;
      next = whole(wanted > SMALL ? large : `${large}:small`, drawnFor(wanted));
      sharp = Math.min(WIDEST, wanted);
      // Whatever the seed has stands in: a softer picture, or the large one in a small place.
      have = kept.get(next.key) ?? kept.get(large);
    }
    if (have) put(have, 'kept');
    if (serves(kept.get(next.key), sharp)) return;
    // A picture already on its way is not asked for again.
    if (order?.key === next.key && order.width === next.width && order.height === next.height) return;
    if (order) order.cancelled = true;
    const mine = (order = next);
    void obtain(seed, mine).then((fetched) => {
      if (order === mine) order = null;
      if (fetched && !mine.cancelled) put(fetched, 'drawn');
    });
  };
  delete canvas.dataset.ready;
  paint();
  const observer = new ResizeObserver(() => {
    window.clearTimeout(settle);
    settle = window.setTimeout(paint, SETTLE_MS);
  });
  observer.observe(canvas);
  return () => {
    if (order) order.cancelled = true;
    window.clearTimeout(settle);
    observer.disconnect();
  };
}

/** How wide a large place is taken to be before one has asked: the window. */
const largeWidth = (): number => drawnFor(asked.large || window.innerWidth * (window.devicePixelRatio || 1));

let named = 0;
let underway: Order | null = null;

/**
 * Draws and stores what large, small and shallow places will ask of `seeds`, a while after they were last named. Naming
 * them again, or none, stops what the last naming had underway.
 */
async function ahead(seeds: number[]): Promise<void> {
  const mine = ++named;
  if (underway) underway.cancelled = true;
  await new Promise((resolve) => window.setTimeout(resolve, AHEAD_MS));
  for (const seed of seeds.slice(0, AHEAD)) {
    const key = String(seed);
    const orders = [
      whole(`${key}:small`, asked.small && drawnFor(asked.small), true),
      whole(key, largeWidth(), true),
      shallow(key, asked.shallow, asked.shape || 1, true),
    ];
    for (const order of orders) {
      if (mine !== named) return;
      if (!order.width || serves(kept.get(order.key), order.width)) continue;
      if ((await storedWidth(order.key)) >= order.width * 0.8) continue;
      underway = order;
      await obtain(seed, order);
    }
  }
}

const warming = new Map<string, Promise<void>>();
/** The longest `warm` keeps its caller waiting; the picture still comes. */
const WARM_MS = 2500;

/**
 * Has the first seed's large picture kept before a place asks for it and, unless `busy` says the machine has
 * better to do, every seed's pictures drawn ahead. Resolves once the first is kept or cannot be, or has taken too
 * long; never rejects.
 */
export function warm(seeds: number[], busy: boolean): Promise<void> {
  void ahead(busy ? [] : seeds);
  const front = seeds[0];
  if (front === undefined) return Promise.resolve();
  const key = String(front);
  const width = largeWidth();
  if (serves(kept.get(key), width)) return Promise.resolve();
  let warmed = warming.get(key);
  if (!warmed) {
    warmed = obtain(front, whole(key, width)).then(
      () => void warming.delete(key),
      () => void warming.delete(key),
    );
    warming.set(key, warmed);
  }
  return Promise.race([warmed, new Promise<void>((resolve) => window.setTimeout(resolve, WARM_MS))]);
}
