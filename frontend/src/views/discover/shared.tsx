import type { JSX } from 'preact';
import { Icon, type IconName } from '../../ui/Icons';
import { SelectField } from '../../ui/Select';
import type { SegmentedOption } from '../../ui/Segmented';
import type { ContentKind, ContentVersion } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { contentTargets } from './state';

export const KIND_ICON: Record<ContentKind, IconName> = {
  mod: 'puzzle',
  modpack: 'stack',
  resource_pack: 'image',
  shader_pack: 'palette',
};

export const KIND_TABS: SegmentedOption<ContentKind>[] = [
  { value: 'mod', label: 'Mods', icon: KIND_ICON.mod },
  { value: 'modpack', label: 'Modpacks', icon: KIND_ICON.modpack },
  { value: 'resource_pack', label: 'Resource packs', icon: KIND_ICON.resource_pack },
  { value: 'shader_pack', label: 'Shaders', icon: KIND_ICON.shader_pack },
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

export function isAddable(kind: ContentKind): boolean {
  return kind !== 'modpack';
}

export function versionFits(version: ContentVersion, kind: ContentKind, instance: EnrichedInstance | null): boolean {
  if (!instance) return true;
  const display = instance.version_display;
  if (display.minecraft_label !== 'Unknown' && !version.game_versions.includes(display.minecraft_label)) return false;
  if (!usesLoaderFilter(kind) || version.loaders.length === 0) return true;
  return version.loaders.includes(display.loader_key);
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

export function tagLabel(value: string): string {
  return value.replace(/[-_]/g, ' ');
}

export function Spinner({ size = 14 }: { size?: number }): JSX.Element {
  return <span class="cp-discover-spinner" style={{ width: size, height: size }} aria-hidden="true" />;
}

export function SkeletonCard(): JSX.Element {
  return (
    <div class="cp-discover-card" aria-hidden="true">
      <div class="cp-discover-card-icon cp-skeleton" />
      <div class="cp-discover-card-main">
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '52%', height: 13 }} />
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '100%', marginTop: 12 }} />
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '68%' }} />
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '40%', marginTop: 14 }} />
      </div>
    </div>
  );
}

export function ContentIcon({ url, kind, size = 24 }: { url?: string; kind: ContentKind; size?: number }): JSX.Element {
  if (url) return <img src={url} alt="" loading="lazy" />;
  return <Icon name={KIND_ICON[kind]} size={size} />;
}

/** Dropdown of instances that content can be added to; hidden when none fit. */
export function InstanceTargetPicker({
  placeholder,
  width,
  onPick,
}: {
  placeholder: string;
  width: number;
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
      width={width}
    />
  );
}
