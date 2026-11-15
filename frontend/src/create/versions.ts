import { LOADER_COMPONENT_IDS, loaderKeyFromComponentId, type Channel, type LoaderKey } from './defaults';
import type { CreateSourceId, CreateVersionRow } from './contract';

export type VersionDownloadState = 'none' | 'base' | 'full';

export interface VersionRowTagModel {
  id: string;
  label: string;
}

export interface VersionRowModel {
  id: string;
  sourceId: CreateSourceId;
  selectionId: string;
  displayName: string;
  hint: string | null;
  channel: Channel;
  tags: VersionRowTagModel[];
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

export function loaderKeyFromSourceId(sourceId: CreateSourceId): LoaderKey {
  return sourceId === 'vanilla' ? 'vanilla' : loaderKeyFromComponentId(sourceId);
}

export function sourceIdFromLoaderKey(loader: LoaderKey): CreateSourceId {
  return loader === 'vanilla' ? 'vanilla' : LOADER_COMPONENT_IDS[loader];
}

export function normalizeChannel(value: string | undefined): Channel {
  return CHANNEL_ORDER.includes(value as Channel) ? (value as Channel) : 'unknown';
}

export function versionRow(row: CreateVersionRow): VersionRowModel {
  return {
    id: row.minecraft_version_id,
    sourceId: row.source_id,
    selectionId: row.selection_id,
    displayName: row.display_name,
    hint: row.hint ?? null,
    channel: normalizeChannel(row.channel),
    tags: row.tags,
    downloadState: row.download_state === 'base' || row.download_state === 'full' ? row.download_state : 'none',
    createEnabled: row.create_enabled,
    disabledReason: row.disabled_reason ?? null,
  };
}
