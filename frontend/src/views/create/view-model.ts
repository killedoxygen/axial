import type { CatalogVersion } from '../../types';
import { channelOfVersion, type Channel, type LoaderKey } from './defaults';
import { normalizeVersionDisplay, releaseAnchorsFor } from '../../version-display';

export interface VersionRowModel {
  id: string;
  displayName: string;
  hint: string | null;
  channel: Channel;
  installed: boolean;
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
  releaseAnchors: CatalogVersion[],
  installedSet: Set<string>,
  source: LoaderKey,
): VersionRowModel {
  const display = normalizeVersionDisplay(version, releaseAnchors);
  return {
    id: version.id,
    displayName: display.displayName === version.id ? version.id : display.displayName,
    hint: display.hint,
    channel: channelOfVersion(version),
    installed: source === 'vanilla' && (version.installed || installedSet.has(version.id)),
  };
}

export { releaseAnchorsFor };
