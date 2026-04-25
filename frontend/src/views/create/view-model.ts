import type { CatalogVersion } from '../../types';
import { isReleaseVersion, isSnapshotVersion, parseVersionDisplay } from '../../utils';
import { channelOfVersion, type Channel, type LoaderKey } from './defaults';

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
};

export const CHANNEL_ORDER: Channel[] = ['release', 'snapshot', 'legacy'];

export function buildRowModel(
  version: CatalogVersion,
  releaseAnchors: CatalogVersion[],
  installedSet: Set<string>,
  source: LoaderKey,
): VersionRowModel {
  const display = parseVersionDisplay(version.id, version, releaseAnchors);
  let hint = display.hint;
  if (!hint && isSnapshotVersion(version) && version.release_time) {
    const releaseTime = version.release_time;
    let nearest: CatalogVersion | null = null;
    for (const anchor of releaseAnchors) {
      if ((anchor.release_time || '') >= releaseTime) {
        nearest = anchor;
        break;
      }
    }
    if (!nearest && releaseAnchors.length > 0) {
      nearest = releaseAnchors[releaseAnchors.length - 1] ?? null;
    }
    if (nearest && !version.id.includes(nearest.id)) {
      hint = `~ ${nearest.id}`;
    }
  }
  return {
    id: version.id,
    displayName: display.name === version.id ? version.id : display.name,
    hint: hint && hint !== display.name ? hint : null,
    channel: channelOfVersion(version),
    installed: source === 'vanilla' && (version.installed || installedSet.has(version.id)),
  };
}

export function releaseAnchorsFor(versions: CatalogVersion[]): CatalogVersion[] {
  return versions
    .filter(isReleaseVersion)
    .slice()
    .sort((left, right) => (left.release_time || '').localeCompare(right.release_time || ''));
}
