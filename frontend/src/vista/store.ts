/**
 * Drawn pictures on disk, packed, so a launch shows them without drawing. Best effort throughout: a store that
 * cannot be opened, read or written only means the picture is drawn again.
 */
const DATABASE = 'axial-vista';
const PICTURES = 'pictures';
const RECENT = 'recent';
const WIDTHS = 'widths';
/** Raise when the same scene would be drawn or stored differently: everything an earlier one stored is dropped. */
const DRAWING = 4;

interface Stored {
  key: string;
  at: number;
  bytes: number;
  width: number;
  file: ArrayBuffer;
}

let opening: Promise<IDBDatabase | null> | null = null;

function database(): Promise<IDBDatabase | null> {
  opening ??= new Promise((resolve) => {
    try {
      const request = indexedDB.open(DATABASE, DRAWING);
      request.onupgradeneeded = () => {
        const db = request.result;
        for (const name of db.objectStoreNames) db.deleteObjectStore(name);
        const pictures = db.createObjectStore(PICTURES, { keyPath: 'key' });
        pictures.createIndex(RECENT, ['at', 'bytes']);
        pictures.createIndex(WIDTHS, ['key', 'width']);
      };
      request.onerror = request.onblocked = () => resolve(null);
      request.onsuccess = () => resolve(request.result);
    } catch {
      // Storage is refused here.
      resolve(null);
    }
  });
  return opening;
}

/** One reading of the pictures: undefined where they cannot be read. */
async function read<T>(ask: (pictures: IDBObjectStore) => IDBRequest<T>): Promise<T | undefined> {
  const db = await database();
  return new Promise((resolve) => {
    try {
      const request = ask(db!.transaction(PICTURES).objectStore(PICTURES));
      request.onerror = () => resolve(undefined);
      request.onsuccess = () => resolve(request.result);
    } catch {
      resolve(undefined);
    }
  });
}

export async function stored(key: string): Promise<ArrayBuffer | undefined> {
  return (await read<Stored | undefined>((pictures) => pictures.get(key)))?.file;
}

/** How wide the picture stored under `key` is, read from the index alone; 0 where there is none. */
export async function storedWidth(key: string): Promise<number> {
  const entry = await read((pictures) =>
    pictures.index(WIDTHS).openKeyCursor(IDBKeyRange.bound([key, 0], [key, Infinity])),
  );
  return (entry?.key as [string, number] | undefined)?.[1] ?? 0;
}

/**
 * Stores a picture `width` across, then drops the oldest pictures beyond `budget` bytes; their sizes are read from
 * the index alone.
 */
export async function store(key: string, file: ArrayBuffer, width: number, budget: number): Promise<void> {
  const db = await database();
  try {
    const pictures = db!.transaction(PICTURES, 'readwrite').objectStore(PICTURES);
    const entry: Stored = { key, at: Date.now(), bytes: file.byteLength, width, file };
    pictures.put(entry);
    let bytes = 0;
    const walk = pictures.index(RECENT).openKeyCursor(null, 'prev');
    walk.onsuccess = () => {
      const cursor = walk.result;
      if (!cursor) return;
      bytes += (cursor.key as [number, number])[1];
      if (bytes > budget && cursor.primaryKey !== key) pictures.delete(cursor.primaryKey);
      cursor.continue();
    };
  } catch {
    // Unavailable, out of room or closing: the picture is drawn again next time.
  }
}
