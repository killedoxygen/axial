import { store, stored, storedWidth } from './store';
import { draw, packed, unpacked, type Scene } from './studio';

const ASPECT = 2.3;
const PIXELS = 2_400_000;
const WIDEST = Math.floor(Math.sqrt(PIXELS * ASPECT));
/** A place this wide or narrower, in device pixels, gets a picture of its own size: a large one shrinks badly. */
const SMALL = 960;
/** What the kept pictures may hold between them in memory: six of the largest. */
const KEPT_BYTES = PIXELS * 4 * 6;
/** How many scenes have their pictures drawn ahead of any place asking. */
const AHEAD = 12;
/** What the store may hold, packed: those scenes' pictures in both colour modes at the largest size, and some over. */
const STORED_BYTES = 64 * 1024 * 1024;
/** How long after the scenes to draw ahead were last named the drawing begins. */
const AHEAD_MS = 1500;
/** How long a place's size must hold still before its picture is drawn for it. */
const SETTLE_MS = 400;

/**
 * One whole picture is kept for a scene and every large place that shows it shares it: Home, the instance page,
 * and the same place at another window size. Each scales and crops it in its stylesheet, so a change of page or of
 * size shows a picture at once. It is drawn as wide as the place that first asks, and again only for a place so
 * much wider that scaling would soften it.
 *
 * Where the widest whole picture is still soft, a shallow place then gets a band: just the rows it shows, at its
 * own sharpness. The band's key carries where the place sets the picture, as its stylesheet's `object-position`.
 *
 * A picture is stored as it is drawn, and one not kept is read from there before it is drawn. The scenes named to
 * `warm` are drawn and stored ahead, so a place seldom waits for a drawing at all.
 */
const kept = new Map<string, ImageData>();
/** How wide the last large and the last small place asked for their pictures, in device pixels: what is drawn ahead. */
const asked = { large: 0, small: 0 };

/** A picture to have: how large, of what `shape`, and for a band, where its horizon lies. */
interface Order {
  key: string;
  width: number;
  height: number;
  shape: number;
  horizon?: number;
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
  shape: ASPECT,
  cancelled: false,
  ahead,
});

const keyOf = ({ seed, version, dark }: Scene): string => `${seed}:${version}:${dark}`;

/** How wide a whole picture is drawn for a place `wanted` device pixels across. */
const drawnFor = (wanted: number): number => Math.min(WIDEST, Math.max(320, Math.ceil(wanted / 64) * 64));

/** Whether `picture` is too shallow to hold the rows a place of `shape` shows. */
const lacks = (picture: ImageData, shape: number): boolean => picture.width > picture.height * shape * 1.002;

/** Whether `picture` is sharp at `width` and, for a band, holds the rows a place of `shape` shows. */
const serves = (picture: ImageData | undefined, width: number, shape = ASPECT): picture is ImageData =>
  !!picture && picture.width >= width * 0.8 && !lacks(picture, shape);

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
async function obtain(scene: Scene, order: Order): Promise<ImageData | undefined> {
  for (;;) {
    await turn(order);
    try {
      if (order.cancelled) return undefined;
      let picture = kept.get(order.key);
      if (serves(picture, order.width, order.shape)) return picture;
      const file = await stored(order.key);
      picture = file && (await unpacked(file));
      if (!serves(picture, order.width, order.shape)) {
        const stopped = (): boolean => order.cancelled || outranked;
        picture = (await draw(stopped, scene, order.width, order.height, order.horizon)) ?? undefined;
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
 * Shows a scene on `canvas` and keeps it right for the canvas's size until the returned function is called. A kept
 * picture is on the canvas before this returns; a softer one stands in while a sharper one is fetched.
 */
export function show(canvas: HTMLCanvasElement, scene: Scene): () => void {
  const large = keyOf(scene);
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
    const shape = box.width / box.height;
    // How wide the whole picture stands here: a place narrower than the picture crops its sides.
    const wanted = Math.max(box.width, box.height * ASPECT) * (window.devicePixelRatio || 1);
    if (!(wanted >= 16)) return;
    asked[wanted > SMALL ? 'large' : 'small'] = wanted;
    let next = whole(wanted > SMALL ? large : `${large}:small`, drawnFor(wanted));
    const picture = kept.get(next.key);
    if (serves(picture, Math.min(WIDEST, wanted))) {
      put(picture, 'kept');
      // The band: worth drawing where the whole picture is stretched and the place shows well under all of it.
      if (picture.width >= wanted * 0.8 || shape < ASPECT * 1.3) return;
      const anchor = parseFloat(getComputedStyle(canvas).objectPosition.split(' ')[1]!) / 100;
      const wide = Math.min(Math.ceil(wanted / 64), Math.floor(Math.sqrt(PIXELS * shape) / 64)) * 64;
      const rows = Math.round(wide / shape);
      const horizon = (wide / ASPECT / 2 - anchor * (wide / ASPECT - rows)) / rows;
      if (!(horizon > 0 && horizon < 1)) return;
      next = { key: `${large}:${anchor}`, width: wide, height: rows, shape, horizon, cancelled: false };
      const band = kept.get(next.key);
      if (serves(band, wide, shape)) return put(band, 'kept');
    } else {
      // Whatever the scene has stands in: a softer picture, or the large one in a small place.
      const stand = picture ?? kept.get(large);
      if (stand) put(stand, 'kept');
    }
    // A picture already on its way is not asked for again.
    if (order?.key === next.key && order.width === next.width && order.height === next.height) return;
    if (order) order.cancelled = true;
    const mine = (order = next);
    void obtain(scene, mine).then((fetched) => {
      if (order === mine) order = null;
      if (!fetched || mine.cancelled) return;
      put(fetched, 'drawn');
      // A whole picture just had may still want its band.
      if (mine.horizon === undefined) paint();
    });
  };
  delete canvas.dataset.ready;
  paint();
  const observer = new ResizeObserver(() => {
    // A band holds only the rows its place showed. A place grown deeper goes back to the whole picture at once.
    const box = canvas.getBoundingClientRect();
    const picture = kept.get(large);
    if (shown && picture && lacks(shown, ASPECT) && lacks(shown, box.width / box.height)) put(picture, 'kept');
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
 * Draws and stores what large and small places will ask of `scenes`, a while after they were last named. Naming
 * them again, or none, stops what the last naming had underway.
 */
async function ahead(scenes: Scene[]): Promise<void> {
  const mine = ++named;
  if (underway) underway.cancelled = true;
  await new Promise((resolve) => window.setTimeout(resolve, AHEAD_MS));
  for (const scene of scenes.slice(0, AHEAD)) {
    const key = keyOf(scene);
    const orders = [whole(`${key}:small`, asked.small && drawnFor(asked.small), true), whole(key, largeWidth(), true)];
    for (const order of orders) {
      if (mine !== named) return;
      if (!order.width || serves(kept.get(order.key), order.width)) continue;
      if ((await storedWidth(order.key)) >= order.width * 0.8) continue;
      underway = order;
      await obtain(scene, order);
    }
  }
}

const warming = new Map<string, Promise<void>>();
/** The longest `warm` keeps its caller waiting; the picture still comes. */
const WARM_MS = 2500;

/**
 * Has the first scene's large picture kept before a place asks for it and, unless `busy` says the machine has
 * better to do, every scene's pictures drawn ahead. Resolves once the first is kept or cannot be, or has taken too
 * long; never rejects.
 */
export function warm(scenes: Scene[], busy: boolean): Promise<void> {
  void ahead(busy ? [] : scenes);
  const front = scenes[0];
  if (!front) return Promise.resolve();
  const key = keyOf(front);
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
