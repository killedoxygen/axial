import { useEffect } from 'preact/hooks';
import { cachedView, catalogRevision, listViews, viewsPending } from './catalog';
import type { CreateVersionRow } from '../generated/CreateVersionRow';
import type { LoaderKey } from './defaults';
import { parseVersionQuery, rankVersions } from './search';
import {
  loaderKeyFromSourceId,
  sourceIdFromLoaderKey,
  versionRow,
  type CreateSourceId,
  type VersionRowModel,
} from './versions';

/** Where a typed version is looked for. */
export interface SearchScope {
  /** Where text that names no loader looks. */
  source: CreateSourceId;
  /** Versions the surface rules out, such as ones its staged picks cannot run on. */
  allows?: (row: CreateVersionRow) => boolean;
  /** Loaders the surface rules out. */
  offers?: (loader: LoaderKey) => boolean;
  limit: number;
}

export interface VersionMatches {
  /** Closest first. */
  rows: VersionRowModel[];
  /** The loader the text named, when its versions answered. */
  loader: LoaderKey | null;
  /** A loader the text reaches is still being listed, so the top match may yet change. */
  pending: boolean;
}

/** The versions typed text names, from the loaders listed so far. Any surface that creates instances can ask. */
function findVersions(query: string, scope: SearchScope): VersionMatches & { reaches: CreateSourceId[] } {
  const rowsOf = (source: CreateSourceId): VersionRowModel[] =>
    (cachedView(source)?.versions ?? [])
      .filter((row) => row.source_id === source && (scope.allows?.(row) ?? true))
      .map(versionRow);
  const offered = (cachedView(scope.source)?.sources ?? [])
    .filter((option) => option.enabled)
    .map((option) => loaderKeyFromSourceId(option.id))
    .filter((loader) => scope.offers?.(loader) ?? true);
  const parsed = parseVersionQuery(query, offered);
  const named = parsed.loader ? sourceIdFromLoaderKey(parsed.loader) : null;
  const reaches = named && named !== scope.source ? [scope.source, named] : [scope.source];
  const matches = named ? rankVersions(rowsOf(named), parsed.version) : [];
  // Letters that named a loader but no version of it are read as the version itself.
  const rows = matches.length > 0 ? matches : rankVersions(rowsOf(scope.source), query.trim().toLowerCase());
  return {
    rows: rows.slice(0, scope.limit),
    loader: matches.length > 0 ? parsed.loader : null,
    pending: viewsPending(reaches),
    reaches,
  };
}

/** `findVersions` kept current: it lists the loaders the text reaches and redraws as they arrive. Null when idle. */
export function useVersionSearch(query: string, scope: SearchScope): VersionMatches | null {
  catalogRevision.value;
  const found = query.trim() ? findVersions(query, scope) : null;
  const reaches = found?.reaches.join('\n') ?? '';
  useEffect(() => {
    if (reaches) listViews(reaches.split('\n') as CreateSourceId[]);
  }, [reaches]);
  return found;
}
