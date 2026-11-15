import { signal } from '@preact/signals';
import { api } from '../api';
import { createBackendViewResponse, type CreateBackendViewResponse, type CreateSourceId } from './contract';

/* The last good create view per loader, kept for the session: lists drawn from it appear at once while a fresh
   copy loads, and a search can read every loader it names. */
const views = new Map<CreateSourceId, CreateBackendViewResponse>();
const listing = new Set<CreateSourceId>();

/** Moves whenever a view arrives or a listing settles; read it to be redrawn with the catalog. */
export const catalogRevision = signal(0);

export function cachedView(source: CreateSourceId): CreateBackendViewResponse | null {
  return views.get(source) ?? null;
}

export function storeView(source: CreateSourceId, view: CreateBackendViewResponse): void {
  views.set(source, view);
  catalogRevision.value += 1;
}

export async function fetchView(source: CreateSourceId): Promise<CreateBackendViewResponse> {
  const view = createBackendViewResponse(
    await api('GET', `/instances/create-view?source=${encodeURIComponent(source)}`),
  );
  storeView(source, view);
  return view;
}

/** Lists the loaders not yet cached, in the background. One that cannot be listed just stays absent. */
export function listViews(sources: readonly CreateSourceId[]): void {
  for (const source of sources) {
    if (views.has(source) || listing.has(source)) continue;
    listing.add(source);
    catalogRevision.value += 1;
    fetchView(source)
      .catch(() => undefined)
      .finally(() => {
        listing.delete(source);
        catalogRevision.value += 1;
      });
  }
}

export function viewsPending(sources: readonly CreateSourceId[]): boolean {
  return sources.some((source) => listing.has(source));
}
