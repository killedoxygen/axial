import type { ContentSearchInput } from '../content';
import type { ContentPage, SearchHit } from '../types-content';

export const DISCOVER_SEARCH_FRESH_FOR_MS = 2 * 60 * 1_000;

export interface DiscoverSearchSnapshot {
  loadedSearchKey: string;
  loadedContextKey: string;
  loadedAt: number | null;
  results: SearchHit[];
  total: number;
  loading: boolean;
  loadingMore: boolean;
  searchError: string | null;
}

export interface DiscoverSearchState {
  read: () => DiscoverSearchSnapshot;
  update: (patch: Partial<DiscoverSearchSnapshot>) => void;
}

export interface SearchScheduler {
  set: (callback: () => void, delayMs: number) => number;
  clear: (handle: number) => void;
}

type SearchPage = (input: ContentSearchInput) => Promise<ContentPage>;

export interface DiscoverSearchRequest {
  key: string;
  contextKey: string;
  input: ContentSearchInput;
  search: SearchPage;
  errorMessage: (error: unknown) => string;
  debounceMs?: number;
  force?: boolean;
}

export interface DiscoverLoadMoreRequest {
  key: string;
  input: ContentSearchInput;
  search: SearchPage;
}

export function createDiscoverSearchLifecycle(
  state: DiscoverSearchState,
  scheduler: SearchScheduler = {
    set: (callback, delayMs) => window.setTimeout(callback, delayMs),
    clear: (handle) => window.clearTimeout(handle),
  },
  now: () => number = Date.now,
): {
  search: (request: DiscoverSearchRequest) => void;
  loadMore: (request: DiscoverLoadMoreRequest) => void;
} {
  let generation = 0;
  let scheduled: number | null = null;
  let pendingInitialKey: string | null = null;

  const cancelScheduled = (): void => {
    if (scheduled !== null) scheduler.clear(scheduled);
    scheduled = null;
  };

  const beginSearch = (request: DiscoverSearchRequest): void => {
    const current = state.read();
    if (!request.force) {
      if (pendingInitialKey === request.key && current.loading) return;
      const loadedSearchIsFresh = current.loadedAt !== null && now() - current.loadedAt < DISCOVER_SEARCH_FRESH_FOR_MS;
      if (
        loadedSearchIsFresh &&
        current.loadedSearchKey === request.key &&
        current.loadedContextKey === request.contextKey
      ) {
        if (pendingInitialKey !== null) {
          generation += 1;
          cancelScheduled();
          pendingInitialKey = null;
        }
        state.update({ loading: false, searchError: null });
        return;
      }
    }

    const requestGeneration = ++generation;
    cancelScheduled();
    pendingInitialKey = request.key;
    const contextChanged = current.loadedContextKey !== '' && current.loadedContextKey !== request.contextKey;
    state.update({
      loading: true,
      loadingMore: false,
      searchError: null,
      ...(contextChanged ? { loadedSearchKey: '', loadedContextKey: '', loadedAt: null, results: [], total: 0 } : {}),
    });

    scheduled = scheduler.set(() => {
      scheduled = null;
      void request
        .search(request.input)
        .then((page) => {
          if (requestGeneration !== generation) return;
          state.update({
            loadedSearchKey: request.key,
            loadedContextKey: request.contextKey,
            loadedAt: now(),
            results: page.items,
            total: page.total,
            searchError: null,
          });
        })
        .catch((error: unknown) => {
          if (requestGeneration === generation) state.update({ searchError: request.errorMessage(error) });
        })
        .finally(() => {
          if (requestGeneration !== generation) return;
          pendingInitialKey = null;
          state.update({ loading: false });
        });
    }, request.debounceMs ?? 0);
  };

  const loadMore = (request: DiscoverLoadMoreRequest): void => {
    const current = state.read();
    if (
      current.loading ||
      current.loadingMore ||
      current.loadedSearchKey !== request.key ||
      current.results.length >= current.total
    ) {
      return;
    }

    const requestGeneration = generation;
    state.update({ loadingMore: true });
    void request
      .search(request.input)
      .then((page) => {
        const latest = state.read();
        if (requestGeneration !== generation || latest.loadedSearchKey !== request.key) return;
        state.update({ results: [...latest.results, ...page.items], total: page.total });
      })
      .catch(() => undefined)
      .finally(() => {
        const latest = state.read();
        if (requestGeneration === generation && latest.loadedSearchKey === request.key) {
          state.update({ loadingMore: false });
        }
      });
  };

  return { search: beginSearch, loadMore };
}
