import type { CatalogVersion, Version } from './types';
import { isReleaseVersion, isSnapshotVersion, parseVersionDisplay } from './utils';

type VersionDisplaySource =
  | Pick<Version, 'id' | 'inherits_from' | 'loader' | 'minecraft_meta' | 'lifecycle' | 'release_time'>
  | Pick<CatalogVersion, 'id' | 'minecraft_meta' | 'lifecycle' | 'release_time'>;

export interface NormalizedVersionDisplay {
  displayName: string;
  hint: string | null;
  minecraftLabel: string;
  searchText: string;
}

function isVersionTokenPrefix(anchorId: string, versionId: string): boolean {
  const anchorTokens = anchorId.split('.');
  const versionTokens = versionId.split(/[.\-_]/);
  return versionTokens.length >= anchorTokens.length
    && anchorTokens.every((token, index) => versionTokens[index] === token);
}

export function releaseAnchorsFor(versions: CatalogVersion[]): CatalogVersion[] {
  return versions
    .filter(isReleaseVersion)
    .slice()
    .sort((left, right) => (left.release_time || '').localeCompare(right.release_time || ''));
}

function inferNeoForgeMinecraftVersion(loaderVersion: string): string {
  const numericParts = loaderVersion
    .split('.')
    .map((part) => {
      const match = part.match(/^\d+/);
      return match?.[0] ?? '';
    })
    .filter(Boolean);
  const major = numericParts[0];
  const minor = numericParts[1];
  if (!major || !minor) return '';

  const majorNumber = Number.parseInt(major, 10);
  if (Number.isFinite(majorNumber) && majorNumber >= 25) {
    const patch = numericParts[2];
    return patch && patch !== '0' ? `${major}.${minor}.${patch}` : `${major}.${minor}`;
  }

  return minor === '0' ? `1.${major}` : `1.${major}.${minor}`;
}

function inferLoaderMinecraftVersion(versionId: string): string {
  const lower = versionId.toLowerCase();

  if (lower.startsWith('fabric-loader-')) {
    const rest = versionId.slice('fabric-loader-'.length);
    const index = rest.lastIndexOf('-');
    return index >= 0 ? rest.slice(index + 1) : '';
  }
  if (lower.startsWith('quilt-loader-')) {
    const rest = versionId.slice('quilt-loader-'.length);
    const index = rest.lastIndexOf('-');
    return index >= 0 ? rest.slice(index + 1) : '';
  }
  const forgeIndex = lower.lastIndexOf('-forge-');
  if (forgeIndex > 0) {
    return versionId.slice(0, forgeIndex);
  }
  if (lower.startsWith('neoforge-')) {
    return inferNeoForgeMinecraftVersion(versionId.slice('neoforge-'.length));
  }
  return '';
}

function loaderMinecraftVersion(version: VersionDisplaySource): string {
  const inherited = 'inherits_from' in version ? version.inherits_from?.trim() ?? '' : '';
  if (inherited) return inherited;
  return inferLoaderMinecraftVersion(version.id);
}

export function normalizeVersionDisplay(
  version: VersionDisplaySource | null | undefined,
  releaseAnchors: VersionDisplaySource[] = [],
): NormalizedVersionDisplay {
  if (!version) {
    return {
      displayName: '',
      hint: null,
      minecraftLabel: '',
      searchText: '',
    };
  }

  const display = parseVersionDisplay(version.id, version, releaseAnchors);
  let hint = display.hint;
  if (!hint && isSnapshotVersion(version) && version.release_time) {
    const releaseTime = version.release_time;
    let nearest: VersionDisplaySource | null = null;
    for (const anchor of releaseAnchors) {
      if ((anchor.release_time || '') >= releaseTime) {
        nearest = anchor;
        break;
      }
    }
    if (!nearest && releaseAnchors.length > 0) {
      nearest = releaseAnchors[releaseAnchors.length - 1] ?? null;
    }
    if (nearest && !isVersionTokenPrefix(nearest.id, version.id)) {
      hint = `~ ${nearest.id}`;
    }
  }

  const displayName = display.name || version.id;
  const meta = version.minecraft_meta;
  const loaderTarget = loaderMinecraftVersion(version);
  const minecraftLabel = loaderTarget
    || meta.effective_version
    || meta.base_id
    || meta.display_name
    || meta.display_hint
    || version.id;
  const searchText = [
    version.id,
    displayName,
    hint,
    minecraftLabel,
    meta.display_name,
    meta.display_hint,
    meta.effective_version,
    meta.base_id,
    loaderTarget,
  ]
    .filter((value): value is string => Boolean(value))
    .join(' ')
    .toLowerCase();

  return {
    displayName,
    hint: hint && hint !== displayName ? hint : null,
    minecraftLabel,
    searchText,
  };
}

export function versionSearchText(
  version: VersionDisplaySource | null | undefined,
  releaseAnchors: VersionDisplaySource[] = [],
): string {
  return normalizeVersionDisplay(version, releaseAnchors).searchText;
}

export function minecraftVersionLabel(
  version: VersionDisplaySource | null | undefined,
  fallback = 'unknown',
): string {
  return normalizeVersionDisplay(version).minecraftLabel || fallback;
}

export function fullVersionLabel(
  version: VersionDisplaySource | null | undefined,
  fallback = 'unknown',
): string {
  const display = normalizeVersionDisplay(version);
  return display.hint
    ? `${display.displayName} (${display.hint})`
    : display.displayName || fallback;
}
