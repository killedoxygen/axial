import assert from 'node:assert/strict';
import test from 'node:test';
import type { ContentPage, SearchHit } from '../src/types-content';
import {
  createDiscoverSearchLifecycle,
  type DiscoverLoadMoreRequest,
  type DiscoverSearchRequest,
} from '../src/machines/discover-search';

interface TestState {
  loadedSearchKey: string;
  loadedContextKey: string;
  loadedAt: number | null;
  results: SearchHit[];
  total: number;
  loading: boolean;
  loadingMore: boolean;
  searchError: string | null;
}

function hit(id: string): SearchHit {
  return {
    canonical_id: id,
    kind: 'mod',
    provider: 'modrinth',
    project_id: id,
    title: id,
    author: 'author',
    summary: id,
    downloads: 0,
    follows: 0,
    categories: [],
    game_versions: [],
    loaders: [],
  };
}

function page(ids: string[], total = ids.length): ContentPage {
  return { items: ids.map(hit), offset: 0, limit: 40, total };
}

function resultIds(state: TestState): string[] {
  return state.results.map((item) => item.canonical_id);
}

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function fakeScheduler(): {
  scheduler: { set: (callback: () => void) => number; clear: (handle: number) => void };
  runNext: () => void;
  size: () => number;
} {
  let nextHandle = 1;
  const callbacks = new Map<number, () => void>();
  return {
    scheduler: {
      set: (callback) => {
        const handle = nextHandle++;
        callbacks.set(handle, callback);
        return handle;
      },
      clear: (handle) => {
        callbacks.delete(handle);
      },
    },
    runNext: () => {
      const entry = callbacks.entries().next().value as [number, () => void] | undefined;
      assert.ok(entry, 'expected a scheduled search');
      callbacks.delete(entry[0]);
      entry[1]();
    },
    size: () => callbacks.size,
  };
}

function harness(now: () => number = Date.now) {
  const state: TestState = {
    loadedSearchKey: '',
    loadedContextKey: '',
    loadedAt: null,
    results: [],
    total: 0,
    loading: false,
    loadingMore: false,
    searchError: null,
  };
  const timers = fakeScheduler();
  const lifecycle = createDiscoverSearchLifecycle(
    {
      read: () => state,
      update: (patch) => Object.assign(state, patch),
    },
    timers.scheduler,
    now,
  );
  return { state, timers, lifecycle };
}

function searchRequest(
  key: string,
  search: DiscoverSearchRequest['search'],
  debounceMs = 0,
  contextKey = 'shared-context',
): DiscoverSearchRequest {
  return {
    key,
    contextKey,
    input: { kind: 'mod', query: key },
    search,
    errorMessage: (error) => String(error),
    debounceMs,
  };
}

async function settle(): Promise<void> {
  for (let index = 0; index < 5; index += 1) await Promise.resolve();
}

test('completed search is reused on remount', async () => {
  const { state, timers, lifecycle } = harness();
  let calls = 0;
  const request = searchRequest('cached', async () => {
    calls += 1;
    return page(['cached']);
  });

  lifecycle.search(request);
  timers.runNext();
  await settle();
  lifecycle.search(request);

  assert.equal(calls, 1);
  assert.equal(timers.size(), 0);
  assert.equal(state.loadedSearchKey, 'cached');
  assert.equal(state.loading, false);
  assert.deepEqual(
    state.results.map((item) => item.canonical_id),
    ['cached'],
  );
});

test('in-flight initial search is shared across remount', async () => {
  const { state, timers, lifecycle } = harness();
  const response = deferred<ContentPage>();
  let calls = 0;
  const request = searchRequest('pending', () => {
    calls += 1;
    return response.promise;
  });

  lifecycle.search(request);
  timers.runNext();
  lifecycle.search(request);

  assert.equal(calls, 1);
  assert.equal(state.loading, true);
  response.resolve(page(['pending']));
  await settle();

  assert.equal(state.loading, false);
  assert.equal(state.loadedSearchKey, 'pending');
});

test('load-more completion clears its flag after remount', async () => {
  const { state, timers, lifecycle } = harness();
  const initialRequest = searchRequest('paged', async () => page(['first'], 2));
  lifecycle.search(initialRequest);
  timers.runNext();
  await settle();

  const response = deferred<ContentPage>();
  const moreRequest: DiscoverLoadMoreRequest = {
    key: 'paged',
    input: { kind: 'mod', query: 'paged', offset: 1 },
    search: () => response.promise,
  };
  lifecycle.loadMore(moreRequest);
  lifecycle.search(initialRequest);

  assert.equal(state.loadingMore, true);
  response.resolve(page(['second'], 2));
  await settle();

  assert.equal(state.loadingMore, false);
  assert.deepEqual(
    state.results.map((item) => item.canonical_id),
    ['first', 'second'],
  );
});

test('query transitions keep old results and exclude stale responses', async () => {
  const { state, timers, lifecycle } = harness();
  lifecycle.search(searchRequest('alpha', async () => page(['alpha'])));
  timers.runNext();
  await settle();

  const beta = deferred<ContentPage>();
  lifecycle.search(searchRequest('beta', () => beta.promise, 220));

  assert.equal(state.loading, true);
  assert.deepEqual(
    state.results.map((item) => item.canonical_id),
    ['alpha'],
  );
  timers.runNext();

  lifecycle.search(searchRequest('alpha', async () => page(['unexpected'])));
  assert.equal(state.loading, false);
  assert.equal(timers.size(), 0);
  assert.deepEqual(
    state.results.map((item) => item.canonical_id),
    ['alpha'],
  );

  beta.resolve(page(['stale-beta']));
  await settle();

  assert.equal(state.loadedSearchKey, 'alpha');
  assert.deepEqual(
    state.results.map((item) => item.canonical_id),
    ['alpha'],
  );
});

test('context transitions hide stale actionable results immediately', async () => {
  const { state, timers, lifecycle } = harness();
  lifecycle.search(searchRequest('target-a', async () => page(['installed-for-a']), 0, 'instance-a'));
  timers.runNext();
  await settle();

  lifecycle.search(searchRequest('target-b', async () => page(['compatible-with-b']), 220, 'instance-b'));

  assert.equal(state.loading, true);
  assert.equal(state.loadedSearchKey, '');
  assert.equal(state.loadedContextKey, '');
  assert.deepEqual(state.results, []);

  timers.runNext();
  await settle();
  assert.equal(state.loadedContextKey, 'instance-b');
  assert.deepEqual(resultIds(state), ['compatible-with-b']);
});

test('completed searches revalidate after the freshness window', async () => {
  let clock = 0;
  const { state, timers, lifecycle } = harness(() => clock);
  let calls = 0;
  const request = searchRequest('freshness', async () => page([`result-${++calls}`]));

  lifecycle.search(request);
  timers.runNext();
  await settle();

  clock = 2 * 60 * 1_000 - 1;
  lifecycle.search(request);
  assert.equal(timers.size(), 0);
  assert.equal(calls, 1);

  clock += 1;
  lifecycle.search(request);
  assert.equal(state.loading, true);
  assert.deepEqual(resultIds(state), ['result-1']);
  timers.runNext();
  await settle();
  assert.equal(calls, 2);
  assert.deepEqual(resultIds(state), ['result-2']);
});

test('initial rejection settles loading and exposes the error', async () => {
  const { state, timers, lifecycle } = harness();
  lifecycle.search(
    searchRequest('broken', async () => {
      throw new Error('offline');
    }),
  );
  timers.runNext();
  await settle();

  assert.equal(state.loading, false);
  assert.match(state.searchError ?? '', /offline/);
});

test('pagination rejection clears loading-more without losing results', async () => {
  const { state, timers, lifecycle } = harness();
  lifecycle.search(searchRequest('paged-error', async () => page(['first'], 2)));
  timers.runNext();
  await settle();

  lifecycle.loadMore({
    key: 'paged-error',
    input: { kind: 'mod', query: 'paged-error', offset: 1 },
    search: async () => {
      throw new Error('offline');
    },
  });
  await settle();

  assert.equal(state.loadingMore, false);
  assert.deepEqual(resultIds(state), ['first']);
});

test('forced retry supersedes a same-key in-flight request', async () => {
  const { state, timers, lifecycle } = harness();
  const first = deferred<ContentPage>();
  const request = searchRequest('retry', () => first.promise);
  lifecycle.search(request);
  timers.runNext();

  lifecycle.search({ ...request, force: true, search: async () => page(['retry-result']) });
  timers.runNext();
  await settle();
  first.resolve(page(['stale-result']));
  await settle();

  assert.equal(state.loading, false);
  assert.deepEqual(resultIds(state), ['retry-result']);
});
