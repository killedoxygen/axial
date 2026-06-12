import type { CatalogVersion } from '../../types';
import { channelOfVersion, type Channel, type LoaderKey } from './defaults';
import { normalizeVersionDisplay } from '../../version-display';

export type VersionDownloadState = 'none' | 'base' | 'full';

export interface VersionRowModel {
  id: string;
  displayName: string;
  hint: string | null;
  channel: Channel;
  downloadState: VersionDownloadState;
}

export const CHANNEL_LABEL: Record<Channel, string> = {
  release: 'Release',
  snapshot: 'Snapshot',
  legacy: 'Legacy',
  unknown: 'Unknown',
};

export const CHANNEL_ORDER: Channel[] = ['release', 'snapshot', 'legacy', 'unknown'];

export function buildRowModel(
  version: CatalogVersion,
  installedSet: Set<string>,
  source: LoaderKey,
  fullInstalledSet: Set<string> = new Set(),
): VersionRowModel {
  const display = normalizeVersionDisplay(version);
  const baseInstalled = version.installed || installedSet.has(version.id);
  const fullInstalled = source === 'vanilla'
    ? baseInstalled
    : fullInstalledSet.has(version.id);
  return {
    id: version.id,
    displayName: display.displayName === version.id ? version.id : display.displayName,
    hint: display.hint,
    channel: channelOfVersion(version),
    downloadState: fullInstalled ? 'full' : baseInstalled ? 'base' : 'none',
  };
}
