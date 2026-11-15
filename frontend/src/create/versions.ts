import { LOADER_COMPONENT_IDS, loaderKeyFromComponentId, type Channel, type LoaderKey } from './defaults';
import type { CreateLoaderBuildsView } from '../generated/CreateLoaderBuildsView';
import type { CreateVersionRow } from '../generated/CreateVersionRow';
import type { LoaderComponentId } from '../generated/LoaderComponentId';

/** A create source: plain Minecraft or one loader. */
export type CreateSourceId = 'vanilla' | LoaderComponentId;

export type VersionDownloadState = 'none' | 'base' | 'full';

export interface VersionRowModel {
  id: string;
  sourceId: CreateSourceId;
  selectionId: string;
  displayName: string;
  hint: string | null;
  channel: Channel;
  downloadState: VersionDownloadState;
  createEnabled: boolean;
  disabledReason: string | null;
}

export const CHANNEL_LABEL: Record<Channel, string> = {
  release: 'Release',
  snapshot: 'Snapshot',
  legacy: 'Legacy',
  unknown: 'Other',
};

export const CHANNEL_ORDER: Channel[] = ['release', 'snapshot', 'legacy', 'unknown'];

/** The loader a create source names; the create view's sources are `vanilla` or a loader component id. */
export function loaderKeyFromSourceId(sourceId: string): LoaderKey {
  return sourceId === 'vanilla' ? 'vanilla' : loaderKeyFromComponentId(sourceId as LoaderComponentId);
}

export function sourceIdFromLoaderKey(loader: LoaderKey): CreateSourceId {
  return loader === 'vanilla' ? 'vanilla' : LOADER_COMPONENT_IDS[loader];
}

/** The create filter a version's lifecycle channel files it under. Unknown stays explicit, never guessed from a name. */
export function normalizeChannel(value: string | undefined): Channel {
  switch (value) {
    case 'stable':
    case 'release':
      return 'release';
    case 'preview':
    case 'experimental':
    case 'snapshot':
      return 'snapshot';
    case 'legacy':
      return 'legacy';
    default:
      return 'unknown';
  }
}

export function versionRow(row: CreateVersionRow): VersionRowModel {
  return {
    id: row.minecraft_version_id,
    sourceId: row.source_id as CreateSourceId,
    selectionId: row.selection_id,
    displayName: row.display_name,
    hint: row.hint ?? null,
    channel: normalizeChannel(row.channel),
    downloadState: row.download_state === 'base' || row.download_state === 'full' ? row.download_state : 'none',
    createEnabled: row.create_enabled,
    disabledReason: row.disabled_reason ?? null,
  };
}

/** The selection a create submits: the automatic build, or the pinned one while it is still offered. */
export function createLoaderSelection(
  builds: CreateLoaderBuildsView | null,
  sourceId: CreateSourceId,
  minecraftVersionId: string | null,
  automaticSelection: string,
  pinnedSelection: string | null,
): string {
  if (sourceId === 'vanilla') return automaticSelection;
  if (!builds || builds.source_id !== sourceId || builds.minecraft_version_id !== minecraftVersionId) {
    return '';
  }
  if (!pinnedSelection) return builds.auto.enabled ? builds.auto.selection_id : '';
  return builds.builds.find((build) => build.selection_id === pinnedSelection && build.enabled)?.selection_id ?? '';
}
