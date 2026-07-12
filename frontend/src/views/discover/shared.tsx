import type { JSX } from 'preact';
import { Icon } from '../../ui/Icons';
import type { SegmentedOption } from '../../ui/Segmented';
import type { ContentKind } from '../../types-content';

export const KIND_TABS: SegmentedOption<ContentKind>[] = [
  { value: 'mod', label: 'Mods', icon: 'puzzle' },
  { value: 'modpack', label: 'Modpacks', icon: 'archive' },
  { value: 'resource_pack', label: 'Resource packs', icon: 'image' },
  { value: 'shader_pack', label: 'Shaders', icon: 'sparkles' },
];

export const KIND_ICON: Record<ContentKind, string> = {
  mod: 'puzzle',
  modpack: 'archive',
  resource_pack: 'image',
  shader_pack: 'sparkles',
};

export const KIND_NOUN: Record<ContentKind, string> = {
  mod: 'mod',
  modpack: 'modpack',
  resource_pack: 'resource pack',
  shader_pack: 'shader pack',
};

/** Only mods and modpacks are tagged with a loader upstream. */
export function usesLoaderFilter(kind: ContentKind): boolean {
  return kind === 'mod' || kind === 'modpack';
}

/** A modpack is a whole instance, so it is never added to one. */
export function isAddable(kind: ContentKind): boolean {
  return kind !== 'modpack';
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

export function formatCount(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(value >= 10_000_000 ? 0 : 1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(value >= 10_000 ? 0 : 1)}k`;
  return String(value);
}

export function formatBytes(bytes: number): string {
  if (!bytes) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exponent;
  return `${value.toFixed(value >= 10 || exponent === 0 ? 0 : 1)} ${units[exponent]}`;
}

export function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

export function Spinner({ size = 14 }: { size?: number }): JSX.Element {
  return <span class="cp-discover-spinner" style={{ width: size, height: size }} aria-hidden="true" />;
}

export function SkeletonCard(): JSX.Element {
  return (
    <div class="cp-discover-card cp-discover-card--skeleton" aria-hidden="true">
      <div class="cp-discover-card-icon cp-skeleton" />
      <div class="cp-discover-card-body">
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '60%' }} />
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '35%' }} />
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '100%', marginTop: 8 }} />
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '80%' }} />
      </div>
    </div>
  );
}

export function ContentIcon({ url, kind, size = 22 }: { url?: string; kind: ContentKind; size?: number }): JSX.Element {
  if (url) return <img src={url} alt="" loading="lazy" />;
  return <Icon name={KIND_ICON[kind]} size={size} />;
}
