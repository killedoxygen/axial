import type { JSX } from 'preact';
import { Chip, type TabItem } from '../../ui/Atoms';
import type { IconName } from '../../ui/Icons';
import { SelectField } from '../../ui/Select';
import { formatCount } from '../../format';
import type { ContentKind, ContentVersion } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { contentTargets } from './state';

export const KIND_ICON: Record<ContentKind, IconName> = {
  mod: 'puzzle',
  modpack: 'stack',
  resource_pack: 'image',
  shader_pack: 'palette',
};

export const KIND_TABS: TabItem<ContentKind>[] = [
  { id: 'mod', label: 'Mods', icon: KIND_ICON.mod },
  { id: 'modpack', label: 'Modpacks', icon: KIND_ICON.modpack },
  { id: 'resource_pack', label: 'Resource packs', icon: KIND_ICON.resource_pack },
  { id: 'shader_pack', label: 'Shaders', icon: KIND_ICON.shader_pack },
];

export const KIND_NOUN: Record<ContentKind, string> = {
  mod: 'mod',
  modpack: 'modpack',
  resource_pack: 'resource pack',
  shader_pack: 'shader pack',
};

export function usesLoaderFilter(kind: ContentKind): boolean {
  return kind === 'mod' || kind === 'modpack';
}

export function versionFits(version: ContentVersion, kind: ContentKind, instance: EnrichedInstance | null): boolean {
  if (!instance) return true;
  const display = instance.version_display;
  if (display.minecraft_label !== 'Unknown' && !version.game_versions.includes(display.minecraft_label)) return false;
  if (!usesLoaderFilter(kind) || version.loaders.length === 0) return true;
  return version.loaders.includes(display.loader_key);
}

const CHANNEL_RANK: Record<ContentVersion['channel'], number> = { release: 2, beta: 1, alpha: 0 };

/** The version an add would choose for this instance: the backend prefers releases, then the newest
 * fitting version with one unambiguous file. Display only; the install plan remains authoritative. */
export function installCandidate(
  versions: ContentVersion[],
  kind: ContentKind,
  instance: EnrichedInstance | null,
): ContentVersion | undefined {
  const installable = (version: ContentVersion): boolean => {
    const primaries = version.files.filter((file) => file.primary).length;
    return primaries === 1 || (primaries === 0 && version.files.length === 1);
  };
  return versions
    .filter((version) => installable(version) && versionFits(version, kind, instance))
    .reduce<ContentVersion | undefined>((best, version) => {
      if (!best) return version;
      const rank = CHANNEL_RANK[version.channel] - CHANNEL_RANK[best.channel];
      if (rank !== 0) return rank > 0 ? version : best;
      return (version.published ?? '') > (best.published ?? '') ? version : best;
    }, undefined);
}

export function compareMcDesc(a: string, b: string): number {
  const pa = a.split('.').map(Number);
  const pb = b.split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i += 1) {
    const diff = (pb[i] ?? 0) - (pa[i] ?? 0);
    if (diff !== 0) return diff;
  }
  return 0;
}

/** A provider slug as words: `game-mechanics` reads "Game mechanics". */
export function tagLabel(value: string): string {
  const words = value.replace(/[-_]/g, ' ');
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** A loader and Minecraft version, as the facts an instance shows. */
export function FitFacts({ fit }: { fit: { loaderLabel: string; gameVersion: string } }): JSX.Element {
  return (
    <>
      <Chip>{fit.loaderLabel}</Chip>
      <Chip mono>{fit.gameVersion}</Chip>
    </>
  );
}

/** How used a project is. */
export function ContentStats({ downloads, follows }: { downloads: number; follows: number }): JSX.Element {
  return (
    <span class="cp-discover-stats">
      <Chip tone="bare" icon="download" title={`${downloads.toLocaleString()} downloads`}>
        {formatCount(downloads)}
      </Chip>
      <Chip tone="bare" icon="user" title={`${follows.toLocaleString()} followers`}>
        {formatCount(follows)}
      </Chip>
    </span>
  );
}

/** The instances content can be added to, as a select; nothing when none fit. */
export function InstanceTargetPicker({
  placeholder,
  onPick,
}: {
  placeholder: string;
  onPick: (instanceId: string) => void;
}): JSX.Element | null {
  const options = contentTargets.value.map((instance) => ({
    value: instance.id,
    label: `${instance.name} (${instance.version_display.summary_label})`,
  }));
  if (options.length === 0) return null;
  return (
    <SelectField
      value=""
      onChange={onPick}
      options={[{ value: '', label: placeholder }, ...options]}
      ariaLabel={placeholder}
    />
  );
}
