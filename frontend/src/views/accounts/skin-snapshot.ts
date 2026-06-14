import { addSceneLighting, buildSkinModel } from './skin-model';
import { loadThree, type ThreeModule } from './skin-three-loader';
import { loadBitmap, loadOptionalBitmap } from './skin-textures';
import type { SkinVariant } from './types';

const SNAPSHOT_WIDTH = 320;
const SNAPSHOT_HEIGHT = 378;
const SNAPSHOT_FOV = 34;
const SNAPSHOT_ROTATION = -Math.PI / 9;
const SNAPSHOT_CENTER_Y = 21.4;
const SNAPSHOT_HALF_HEIGHT = 13.2;
const MAX_SNAPSHOT_CACHE_SIZE = 180;
const MAX_STORED_SNAPSHOT_CACHE_SIZE = 320;
const SNAPSHOT_RENDER_VERSION = 2;
const SNAPSHOT_DB_NAME = 'croopor-skin-snapshots';
const SNAPSHOT_DB_VERSION = 1;
const SNAPSHOT_STORE_NAME = 'snapshots';

export type SnapshotSide = 'front' | 'back';
export type SnapshotStatus = 'idle' | 'queued' | 'loading' | 'ready' | 'error';

export interface SkinSnapshotInput {
  cacheKey: string;
  src: string;
  variant: SkinVariant;
  capeSrc?: string;
  textureIdentity?: string;
  capeIdentity?: string;
}

export type SnapshotState = { status: 'idle' | 'queued' | 'loading' | 'error' } | { status: 'ready'; url: string };

interface SnapshotRig {
  THREE: ThreeModule;
  renderer: import('three').WebGLRenderer;
  canvas: HTMLCanvasElement;
}

interface SnapshotEntry {
  key: string;
  input: SkinSnapshotInput;
  side: SnapshotSide;
  state: SnapshotState;
  listeners: Set<() => void>;
  priority: number;
  requestedAt: number;
  hydrated: boolean;
  hydratePromise: Promise<void> | null;
  generationRequested: boolean;
}

let rigPromise: Promise<SnapshotRig> | null = null;
let dbPromise: Promise<IDBDatabase | null> | null = null;
let running = false;
const entries = new Map<string, SnapshotEntry>();
const queue: SnapshotEntry[] = [];

function snapshotKey(input: SkinSnapshotInput, side: SnapshotSide): string {
  return `v${SNAPSHOT_RENDER_VERSION}:${side}:${input.cacheKey}`;
}

function snapshotDb(): Promise<IDBDatabase | null> {
  if (dbPromise) return dbPromise;
  dbPromise = new Promise((resolve) => {
    if (!('indexedDB' in window)) {
      resolve(null);
      return;
    }

    const request = indexedDB.open(SNAPSHOT_DB_NAME, SNAPSHOT_DB_VERSION);
    request.onupgradeneeded = () => {
      const database = request.result;
      if (!database.objectStoreNames.contains(SNAPSHOT_STORE_NAME)) {
        const store = database.createObjectStore(SNAPSHOT_STORE_NAME, { keyPath: 'key' });
        store.createIndex('updatedAt', 'updatedAt');
      }
    };
    request.onerror = () => resolve(null);
    request.onsuccess = () => resolve(request.result);
  });
  return dbPromise;
}

async function readStoredSnapshotBlob(key: string): Promise<Blob | null> {
  const database = await snapshotDb();
  if (!database) return null;
  return new Promise((resolve) => {
    const transaction = database.transaction(SNAPSHOT_STORE_NAME, 'readonly');
    const request = transaction.objectStore(SNAPSHOT_STORE_NAME).get(key);
    request.onerror = () => resolve(null);
    request.onsuccess = () => {
      const result = request.result as { blob?: unknown } | undefined;
      resolve(result?.blob instanceof Blob ? result.blob : null);
    };
  });
}

async function pruneStoredSnapshots(database: IDBDatabase): Promise<void> {
  await new Promise<void>((resolve) => {
    const transaction = database.transaction(SNAPSHOT_STORE_NAME, 'readwrite');
    const store = transaction.objectStore(SNAPSHOT_STORE_NAME);
    const request = store.getAll();
    request.onerror = () => resolve();
    request.onsuccess = () => {
      const records = (request.result as Array<{ key: string; updatedAt?: number }>).sort(
        (left, right) => (left.updatedAt ?? 0) - (right.updatedAt ?? 0),
      );
      for (const record of records.slice(0, Math.max(0, records.length - MAX_STORED_SNAPSHOT_CACHE_SIZE))) {
        store.delete(record.key);
      }
    };
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => resolve();
  });
}

async function writeStoredSnapshotBlob(key: string, blob: Blob): Promise<void> {
  const database = await snapshotDb();
  if (!database) return;
  await new Promise<void>((resolve) => {
    const transaction = database.transaction(SNAPSHOT_STORE_NAME, 'readwrite');
    transaction.objectStore(SNAPSHOT_STORE_NAME).put({ key, blob, updatedAt: Date.now() });
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => resolve();
  });
  await pruneStoredSnapshots(database);
}

async function snapshotRig(): Promise<SnapshotRig> {
  if (!rigPromise) {
    rigPromise = (async () => {
      const THREE = await loadThree();
      const canvas = document.createElement('canvas');
      canvas.width = SNAPSHOT_WIDTH;
      canvas.height = SNAPSHOT_HEIGHT;
      const renderer = new THREE.WebGLRenderer({
        canvas,
        alpha: true,
        antialias: true,
        preserveDrawingBuffer: true,
      });
      renderer.outputColorSpace = THREE.SRGBColorSpace;
      renderer.setPixelRatio(1);
      renderer.setSize(SNAPSHOT_WIDTH, SNAPSHOT_HEIGHT, false);
      return { THREE, renderer, canvas };
    })();
  }
  return rigPromise;
}

function canvasBlob(canvas: HTMLCanvasElement): Promise<Blob> {
  return new Promise((resolve, reject) => {
    canvas.toBlob((blob) => {
      if (blob) {
        resolve(blob);
        return;
      }
      reject(new Error('Skin snapshot canvas did not produce a PNG.'));
    }, 'image/png');
  });
}

async function renderSnapshot(input: SkinSnapshotInput, side: SnapshotSide): Promise<Blob> {
  const { THREE, renderer, canvas } = await snapshotRig();
  const skinBitmap = await loadBitmap(input.src, input.textureIdentity);
  const capeBitmap = await loadOptionalBitmap(input.capeSrc, 'cape snapshot', input.capeIdentity);
  const disposables: Array<() => void> = [];

  try {
    const scene = new THREE.Scene();
    addSceneLighting(THREE, scene, disposables);

    const group = new THREE.Group();
    group.rotation.y = side === 'back' ? SNAPSHOT_ROTATION + Math.PI : SNAPSHOT_ROTATION;
    scene.add(group);
    const parts = buildSkinModel({
      THREE,
      group,
      skinBitmap,
      capeBitmap,
      variant: input.variant,
      showOuterLayers: true,
      disposables,
    });
    parts.rightArm.rotation.x = 0.1;
    parts.leftArm.rotation.x = -0.1;
    parts.rightArm.rotation.z = 0.03;
    parts.leftArm.rotation.z = -0.03;
    parts.rightLeg.rotation.x = -0.06;
    parts.leftLeg.rotation.x = 0.06;

    const camera = new THREE.PerspectiveCamera(SNAPSHOT_FOV, SNAPSHOT_WIDTH / SNAPSHOT_HEIGHT, 0.1, 500);
    const distance = SNAPSHOT_HALF_HEIGHT / Math.tan(THREE.MathUtils.degToRad(SNAPSHOT_FOV) / 2);
    camera.position.set(0, SNAPSHOT_CENTER_Y, distance);
    camera.lookAt(0, SNAPSHOT_CENTER_Y, 0);
    camera.updateProjectionMatrix();

    renderer.clear();
    renderer.render(scene, camera);
    return canvasBlob(canvas);
  } finally {
    disposables.forEach((dispose) => dispose());
    skinBitmap.close();
    capeBitmap?.close();
  }
}

function notify(entry: SnapshotEntry): void {
  for (const listener of entry.listeners) listener();
}

function setEntryState(entry: SnapshotEntry, state: SnapshotState): void {
  const previous = entry.state;
  if (previous.status === 'ready' && previous.url !== (state.status === 'ready' ? state.url : undefined)) {
    URL.revokeObjectURL(previous.url);
  }
  entry.state = state;
  notify(entry);
}

function setEntryBlob(entry: SnapshotEntry, blob: Blob): void {
  setEntryState(entry, { status: 'ready', url: URL.createObjectURL(blob) });
}

function pruneSnapshotCache(): void {
  if (entries.size <= MAX_SNAPSHOT_CACHE_SIZE) return;
  const removable = [...entries.values()]
    .filter((entry) => entry.listeners.size === 0 && entry.state.status === 'ready')
    .sort((left, right) => left.requestedAt - right.requestedAt);
  for (const entry of removable.slice(0, entries.size - MAX_SNAPSHOT_CACHE_SIZE)) {
    if (entry.state.status === 'ready') URL.revokeObjectURL(entry.state.url);
    entries.delete(entry.key);
  }
}

function idle(callback: () => void): void {
  const requestIdle = (
    window as Window & {
      requestIdleCallback?: (next: () => void, options?: { timeout: number }) => number;
    }
  ).requestIdleCallback;
  if (requestIdle) {
    requestIdle(callback, { timeout: 700 });
    return;
  }
  window.setTimeout(callback, 60);
}

function scheduleQueue(): void {
  if (running || queue.length === 0) return;
  queue.sort((left, right) => right.priority - left.priority || left.requestedAt - right.requestedAt);
  const next = queue[0];
  if (next.priority < 0) {
    idle(processQueue);
    return;
  }
  queueMicrotask(processQueue);
}

function processQueue(): void {
  if (running || queue.length === 0) return;
  queue.sort((left, right) => right.priority - left.priority || left.requestedAt - right.requestedAt);
  const entry = queue.shift();
  if (!entry) return;
  if (entry.state.status === 'ready') {
    scheduleQueue();
    return;
  }
  if (!entry.hydrated) {
    if (!queue.includes(entry)) queue.push(entry);
    return;
  }

  running = true;
  setEntryState(entry, { status: 'loading' });
  void renderSnapshot(entry.input, entry.side)
    .then((blob) => {
      setEntryBlob(entry, blob);
      void writeStoredSnapshotBlob(entry.key, blob);
      pruneSnapshotCache();
    })
    .catch(() => {
      setEntryState(entry, { status: 'error' });
    })
    .finally(() => {
      running = false;
      scheduleQueue();
    });
}

function queueEntry(entry: SnapshotEntry): void {
  if (entry.state.status === 'ready' || entry.state.status === 'loading') return;
  if (!queue.includes(entry)) {
    queue.push(entry);
  }
  scheduleQueue();
}

function hydrateEntry(entry: SnapshotEntry): void {
  if (entry.hydrated || entry.hydratePromise) return;
  entry.hydratePromise = readStoredSnapshotBlob(entry.key)
    .then((blob) => {
      if (blob && entry.state.status !== 'loading') {
        setEntryBlob(entry, blob);
        pruneSnapshotCache();
      }
    })
    .finally(() => {
      entry.hydrated = true;
      entry.hydratePromise = null;
      if (entry.generationRequested && entry.state.status !== 'ready') {
        queueEntry(entry);
      }
    });
}

function ensureEntry(input: SkinSnapshotInput, side: SnapshotSide): SnapshotEntry {
  const key = snapshotKey(input, side);
  const existing = entries.get(key);
  if (existing) {
    existing.input = input;
    existing.requestedAt = Date.now();
    hydrateEntry(existing);
    return existing;
  }
  const entry: SnapshotEntry = {
    key,
    input,
    side,
    state: { status: 'idle' },
    listeners: new Set(),
    priority: 0,
    requestedAt: Date.now(),
    hydrated: false,
    hydratePromise: null,
    generationRequested: false,
  };
  entries.set(key, entry);
  pruneSnapshotCache();
  hydrateEntry(entry);
  return entry;
}

export function getSkinSnapshot(input: SkinSnapshotInput, side: SnapshotSide = 'front'): SnapshotState {
  return ensureEntry(input, side).state;
}

export function requestSkinSnapshot(input: SkinSnapshotInput, side: SnapshotSide = 'front', priority = 0): void {
  const entry = ensureEntry(input, side);
  if (entry.state.status === 'ready' || entry.state.status === 'loading') return;
  entry.generationRequested = true;
  entry.priority = Math.max(entry.priority, priority);
  if (entry.state.status !== 'queued') {
    setEntryState(entry, { status: 'queued' });
  }
  if (entry.hydrated) queueEntry(entry);
}

export function subscribeSkinSnapshot(input: SkinSnapshotInput, side: SnapshotSide, listener: () => void): () => void {
  const entry = ensureEntry(input, side);
  entry.listeners.add(listener);
  return () => {
    entry.listeners.delete(listener);
  };
}
