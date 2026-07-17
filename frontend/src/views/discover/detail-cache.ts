import { getContentDetail } from '../../content';
import type { ContentDetail } from '../../types-content';

const DETAIL_CACHE_MAX = 32;
const DETAIL_FRESH_FOR_MS = 2 * 60 * 1000;
const DETAIL_MAX_AGE_MS = 15 * 60 * 1000;
const DETAIL_PREFETCH_CONCURRENCY = 3;

interface CacheEntry<T> {
  value: T;
  loadedAt: number;
}

interface PrefetchJob {
  key: string;
  listeners: Set<symbol>;
  timer: ReturnType<typeof setTimeout> | null;
  dueAt: number;
  queued: boolean;
  running: boolean;
}

export interface DetailCacheOptions {
  maxEntries?: number;
  freshForMs?: number;
  maxAgeMs?: number;
  maxPrefetchConcurrency?: number;
  now?: () => number;
}

export interface DetailCache<T> {
  cached: (key: string) => T | undefined;
  load: (key: string) => Promise<T>;
  schedulePrefetch: (key: string, delayMs?: number) => () => void;
}

/** Creates a bounded cache with separate cached reads and deduplicated refreshes. */
export function createDetailCache<T>(
  fetchDetail: (key: string) => Promise<T>,
  options: DetailCacheOptions = {},
): DetailCache<T> {
  const maxEntries = Math.max(1, options.maxEntries ?? DETAIL_CACHE_MAX);
  const freshForMs = Math.max(0, options.freshForMs ?? DETAIL_FRESH_FOR_MS);
  const maxAgeMs = Math.max(freshForMs, options.maxAgeMs ?? DETAIL_MAX_AGE_MS);
  const maxPrefetchConcurrency = Math.max(1, options.maxPrefetchConcurrency ?? DETAIL_PREFETCH_CONCURRENCY);
  const now = options.now ?? Date.now;

  const entries = new Map<string, CacheEntry<T>>();
  const inflight = new Map<string, Promise<T>>();
  const prefetchJobs = new Map<string, PrefetchJob>();
  const prefetchQueue: PrefetchJob[] = [];
  let activePrefetches = 0;

  const entry = (key: string, touch: boolean): CacheEntry<T> | undefined => {
    const hit = entries.get(key);
    if (!hit) return undefined;
    if (now() - hit.loadedAt >= maxAgeMs) {
      entries.delete(key);
      return undefined;
    }
    if (touch) {
      entries.delete(key);
      entries.set(key, hit);
    }
    return hit;
  };

  const isFresh = (key: string): boolean => {
    const hit = entry(key, false);
    return hit !== undefined && now() - hit.loadedAt < freshForMs;
  };

  const store = (key: string, value: T): void => {
    entries.delete(key);
    entries.set(key, { value, loadedAt: now() });
    while (entries.size > maxEntries) {
      const oldest = entries.keys().next().value;
      if (oldest === undefined) break;
      entries.delete(oldest);
    }
  };

  const cached = (key: string): T | undefined => entry(key, true)?.value;

  const load = (key: string): Promise<T> => {
    if (!key) return Promise.reject(new Error('A content detail key is required'));

    const hit = entry(key, true);
    if (hit && now() - hit.loadedAt < freshForMs) return Promise.resolve(hit.value);

    const pending = inflight.get(key);
    if (pending) return pending;

    let request: Promise<T>;
    try {
      request = fetchDetail(key);
    } catch (error) {
      return Promise.reject(error);
    }

    const tracked = request
      .then((value) => {
        store(key, value);
        return value;
      })
      .finally(() => {
        if (inflight.get(key) === tracked) inflight.delete(key);
      });
    inflight.set(key, tracked);
    return tracked;
  };

  const finishPrefetch = (job: PrefetchJob): void => {
    activePrefetches -= 1;
    if (prefetchJobs.get(job.key) === job) prefetchJobs.delete(job.key);
    pumpPrefetches();
  };

  const pumpPrefetches = (): void => {
    while (activePrefetches < maxPrefetchConcurrency) {
      const job = prefetchQueue.shift();
      if (!job) return;
      if (prefetchJobs.get(job.key) !== job || job.listeners.size === 0) continue;

      job.queued = false;
      if (isFresh(job.key)) {
        prefetchJobs.delete(job.key);
        continue;
      }

      job.running = true;
      activePrefetches += 1;
      void load(job.key).then(
        () => finishPrefetch(job),
        () => finishPrefetch(job),
      );
    }
  };

  const enqueuePrefetch = (job: PrefetchJob): void => {
    job.timer = null;
    if (prefetchJobs.get(job.key) !== job || job.listeners.size === 0 || job.running || job.queued) return;
    job.queued = true;
    prefetchQueue.push(job);
    pumpPrefetches();
  };

  const armPrefetch = (job: PrefetchJob, delayMs: number): void => {
    const delay = Math.max(0, delayMs);
    job.dueAt = Date.now() + delay;
    if (delay === 0) {
      enqueuePrefetch(job);
      return;
    }
    job.timer = setTimeout(() => enqueuePrefetch(job), delay);
  };

  const schedulePrefetch = (key: string, delayMs = 0): (() => void) => {
    if (!key || isFresh(key)) return () => {};

    const listener = Symbol(key);
    let job = prefetchJobs.get(key);
    if (job) {
      job.listeners.add(listener);
      const requestedDueAt = Date.now() + Math.max(0, delayMs);
      if (job.timer && requestedDueAt < job.dueAt) {
        clearTimeout(job.timer);
        armPrefetch(job, delayMs);
      }
    } else {
      job = {
        key,
        listeners: new Set([listener]),
        timer: null,
        dueAt: 0,
        queued: false,
        running: false,
      };
      prefetchJobs.set(key, job);
      armPrefetch(job, delayMs);
    }

    let cancelled = false;
    return () => {
      if (cancelled) return;
      cancelled = true;
      job?.listeners.delete(listener);
      if (!job || job.listeners.size > 0 || job.running) return;
      if (job.timer) clearTimeout(job.timer);
      if (prefetchJobs.get(key) === job) prefetchJobs.delete(key);
    };
  };

  return { cached, load, schedulePrefetch };
}

const contentDetailCache = createDetailCache(getContentDetail);

export function cachedDetail(canonicalId: string): ContentDetail | undefined {
  return contentDetailCache.cached(canonicalId);
}

export function loadDetail(canonicalId: string): Promise<ContentDetail> {
  return contentDetailCache.load(canonicalId);
}

export function scheduleDetailPrefetch(canonicalId: string, delayMs = 0): () => void {
  return contentDetailCache.schedulePrefetch(canonicalId, delayMs);
}
