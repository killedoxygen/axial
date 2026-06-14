import type { CatalogVersion, Version } from './types';
import { parseVersionDisplay } from './utils';

type VersionDisplaySource =
  | Pick<Version, 'id' | 'inherits_from' | 'loader' | 'minecraft_meta' | 'lifecycle' | 'release_time'>
  | Pick<CatalogVersion, 'id' | 'minecraft_meta' | 'lifecycle' | 'release_time'>;

export interface NormalizedVersionDisplay {
  displayName: string;
  hint: string | null;
  minecraftLabel: string;
  searchText: string;
}

function loaderMinecraftVersion(version: VersionDisplaySource): string {
  return 'inherits_from' in version ? (version.inherits_from?.trim() ?? '') : '';
}

export function normalizeVersionDisplay(version: VersionDisplaySource | null | undefined): NormalizedVersionDisplay {
  if (!version) {
    return {
      displayName: '',
      hint: null,
      minecraftLabel: '',
      searchText: '',
    };
  }

  const display = parseVersionDisplay(version.id, version);
  const hint = display.hint;

  const displayName = display.name || version.id;
  const meta = version.minecraft_meta;
  const loaderTarget = loaderMinecraftVersion(version);
  const minecraftLabel =
    loaderTarget || meta.effective_version || meta.base_id || meta.display_name || meta.display_hint || version.id;
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

export function versionSearchText(version: VersionDisplaySource | null | undefined): string {
  return normalizeVersionDisplay(version).searchText;
}

export function minecraftVersionLabel(version: VersionDisplaySource | null | undefined, fallback = 'unknown'): string {
  return normalizeVersionDisplay(version).minecraftLabel || fallback;
}

export function fullVersionLabel(version: VersionDisplaySource | null | undefined, fallback = 'unknown'): string {
  const display = normalizeVersionDisplay(version);
  return display.hint ? `${display.displayName} (${display.hint})` : display.displayName || fallback;
}
