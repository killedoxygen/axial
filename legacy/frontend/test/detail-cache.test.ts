import assert from 'node:assert/strict';
import test from 'node:test';
import { createDetailCache } from '../src/views/discover/detail-cache';

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (error: unknown) => void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((onResolve, onReject) => {
    resolve = onResolve;
    reject = onReject;
  });
  return { promise, resolve, reject };
}

async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

test('evicts the least recently used detail', async () => {
  const cache = createDetailCache(async (key) => key, { maxEntries: 2 });

  await cache.load('a');
  await cache.load('b');
  assert.equal(cache.cached('a'), 'a');
  await cache.load('c');

  assert.equal(cache.cached('a'), 'a');
  assert.equal(cache.cached('b'), undefined);
  assert.equal(cache.cached('c'), 'c');
});

test('serves fresh details locally and revalidates stale details', async () => {
  let now = 0;
  let version = 0;
  const cache = createDetailCache(async () => ++version, {
    freshForMs: 100,
    maxAgeMs: 500,
    now: () => now,
  });

  assert.equal(await cache.load('a'), 1);
  now = 99;
  assert.equal(await cache.load('a'), 1);
  assert.equal(version, 1);

  now = 100;
  assert.equal(cache.cached('a'), 1);
  assert.equal(await cache.load('a'), 2);

  now = 600;
  assert.equal(cache.cached('a'), undefined);
});

test('deduplicates concurrent requests and retries failures', async () => {
  const first = deferred<string>();
  let calls = 0;
  const cache = createDetailCache(() => {
    calls += 1;
    return calls === 1 ? first.promise : Promise.resolve('recovered');
  });

  const request = cache.load('a');
  assert.equal(cache.load('a'), request);
  first.reject(new Error('offline'));
  await assert.rejects(request, /offline/);

  assert.equal(await cache.load('a'), 'recovered');
  assert.equal(calls, 2);
});

test('bounds speculative requests and cancels queued work', async () => {
  const requests = new Map<string, Deferred<string>>();
  const started: string[] = [];
  const cache = createDetailCache(
    (key) => {
      started.push(key);
      const request = deferred<string>();
      requests.set(key, request);
      return request.promise;
    },
    { maxPrefetchConcurrency: 2 },
  );

  cache.schedulePrefetch('a');
  cache.schedulePrefetch('b');
  const cancelC = cache.schedulePrefetch('c');
  assert.deepEqual(started, ['a', 'b']);

  cancelC();
  requests.get('a')?.resolve('a');
  requests.get('b')?.resolve('b');
  await settle();

  assert.deepEqual(started, ['a', 'b']);
  assert.equal(cache.cached('c'), undefined);
});

test('cancels delayed prefetch and keeps speculative failures retryable', async () => {
  let calls = 0;
  const cache = createDetailCache(async () => {
    calls += 1;
    throw new Error('offline');
  });

  const cancel = cache.schedulePrefetch('cancelled', 10);
  cancel();
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(calls, 0);

  cache.schedulePrefetch('retry');
  await settle();
  cache.schedulePrefetch('retry');
  await settle();
  assert.equal(calls, 2);
});
