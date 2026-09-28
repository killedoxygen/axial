import type { Channel } from './defaults';

export type VersionDownloadState = 'none' | 'base' | 'full';

export interface VersionRowTagModel {
  id: string;
  label: string;
}

export interface VersionRowModel {
  id: string;
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
  unknown: 'Unknown',
};

export const CHANNEL_ORDER: Channel[] = ['release', 'snapshot', 'legacy', 'unknown'];
