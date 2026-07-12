import type { JSX } from 'preact';
import { Icon } from '../../ui/Icons';
import type { SegmentedOption } from '../../ui/Segmented';
import type { ContentKind, ContentVersion } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';

export const KIND_TABS: SegmentedOption<ContentKind>[] = [
  { value: 'mod', label: 'Mods', icon: 'puzzle' },
  { value: 'modpack', label: 'Modpacks', icon: 'stack' },
  { value: 'resource_pack', label: 'Resource packs', icon: 'image' },
  { value: 'shader_pack', label: 'Shaders', icon: 'palette' },
];

export const KIND_ICON: Record<ContentKind, string> = {
  mod: 'puzzle',
  modpack: 'stack',
  resource_pack: 'image',
  shader_pack: 'palette',
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

/**
 * Whether a version can run where the content is headed. Packs and shaders carry
 * no loader tag, so only the Minecraft version has to line up for them.
 */
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

export function formatAge(iso?: string): string {
  if (!iso) return 'unknown';
  const then = Date.parse(iso);
  if (!Number.isFinite(then)) return 'unknown';
  const days = Math.floor((Date.now() - then) / 86_400_000);
  if (days <= 0) return 'today';
  if (days === 1) return 'yesterday';
  if (days < 30) return `${days} days ago`;
  const months = Math.floor(days / 30);
  if (months < 12) return `${months} month${months === 1 ? '' : 's'} ago`;
  const years = Math.floor(months / 12);
  return `${years} year${years === 1 ? '' : 's'} ago`;
}

export function formatDate(iso?: string): string {
  if (!iso) return '';
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return '';
  return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' });
}

export function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

/** Upstream categories arrive as slugs; the UI shows them as words. */
export function tagLabel(value: string): string {
  return value.replace(/[-_]/g, ' ');
}

export function Spinner({ size = 14 }: { size?: number }): JSX.Element {
  return <span class="cp-discover-spinner" style={{ width: size, height: size }} aria-hidden="true" />;
}

export function Stat({ icon, label, value }: { icon: string; label: string; value: string }): JSX.Element {
  return (
    <span class="cp-content-stat" title={label}>
      <Icon name={icon} size={13} />
      <b>{value}</b>
      <span>{label}</span>
    </span>
  );
}

export function SkeletonCard(): JSX.Element {
  return (
    <div class="cp-discover-card cp-discover-card--skeleton" aria-hidden="true">
      <div class="cp-discover-card-open">
        <div class="cp-discover-card-icon cp-skeleton" />
        <div class="cp-discover-card-body">
          <div class="cp-skeleton cp-skeleton-line" style={{ width: '55%', height: 12 }} />
          <div class="cp-skeleton cp-skeleton-line" style={{ width: '30%' }} />
          <div class="cp-skeleton cp-skeleton-line" style={{ width: '100%', marginTop: 10 }} />
          <div class="cp-skeleton cp-skeleton-line" style={{ width: '75%' }} />
        </div>
      </div>
      <div class="cp-discover-card-foot">
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '100%', height: 30, margin: 0 }} />
      </div>
    </div>
  );
}

export function ContentIcon({ url, kind, size = 22 }: { url?: string; kind: ContentKind; size?: number }): JSX.Element {
  if (url) return <img src={url} alt="" loading="lazy" />;
  return <Icon name={KIND_ICON[kind]} size={size} />;
}
