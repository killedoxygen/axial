import { computed, signal } from '@preact/signals';
import { instances } from '../../store';
import { route } from '../../ui-state';
import type { ContentKind, ContentSelection, ContentSort, SearchHit } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';

export const query = signal('');
export const kind = signal<ContentKind>('mod');
export const loader = signal('');
export const gameVersion = signal('');
export const category = signal('');
export const sort = signal<ContentSort>('relevance');

export const results = signal<SearchHit[]>([]);
export const total = signal(0);
export const loading = signal(false);
export const loadingMore = signal(false);
export const searchError = signal<string | null>(null);

export interface TrayItem {
  canonical_id: string;
  kind: ContentKind;
  title: string;
  icon_url?: string;
  version_id?: string;
  version_label?: string;
}

export const tray = signal<TrayItem[]>([]);

export const targetInstance = computed<EnrichedInstance | null>(() => {
  const r = route.value;
  const id = r.name === 'discover' || r.name === 'content' ? r.target : undefined;
  if (!id) return null;
  return (instances.value as EnrichedInstance[]).find((instance) => instance.id === id) ?? null;
});

export const contentTargets = computed<EnrichedInstance[]>(() =>
  (instances.value as EnrichedInstance[]).filter((instance) => instance.version_display.minecraft_label !== 'Unknown'),
);

export function isStaged(canonicalId: string): boolean {
  return tray.value.some((item) => item.canonical_id === canonicalId);
}

export function stagedItem(canonicalId: string): TrayItem | undefined {
  return tray.value.find((item) => item.canonical_id === canonicalId);
}

export function stage(item: TrayItem): void {
  const rest = tray.value.filter((existing) => existing.canonical_id !== item.canonical_id);
  tray.value = [...rest, item];
}

/** Stage a search hit or detail record, optionally pinned to a version. */
export function stageContent(
  item: { canonical_id: string; kind: ContentKind; title: string; icon_url?: string },
  version?: { id: string; version_number: string },
): void {
  stage({
    canonical_id: item.canonical_id,
    kind: item.kind,
    title: item.title,
    icon_url: item.icon_url,
    ...(version ? { version_id: version.id, version_label: version.version_number } : {}),
  });
}

export function unstage(canonicalId: string): void {
  tray.value = tray.value.filter((item) => item.canonical_id !== canonicalId);
}

export function clearTray(): void {
  tray.value = [];
}

export function traySelections(): ContentSelection[] {
  return tray.value.map((item) => ({
    canonical_id: item.canonical_id,
    kind: item.kind,
    ...(item.version_id ? { version_id: item.version_id } : {}),
  }));
}

export function markInstalled(canonicalIds: string[]): void {
  const installed = new Set(canonicalIds);
  results.value = results.value.map((hit) =>
    installed.has(hit.canonical_id) ? { ...hit, install_state: 'installed' } : hit,
  );
}

export function resetSearch(): void {
  results.value = [];
  total.value = 0;
  searchError.value = null;
}
