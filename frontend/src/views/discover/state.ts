import { computed, signal } from '@preact/signals';
import { instances } from '../../store';
import { route } from '../../ui-state';
import type { ContentKind, ContentSelection, ContentSort, SearchHit } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';

/**
 * Discover's search state lives here rather than in the component so that
 * opening a content page and coming back does not throw away your results,
 * scroll position or filters.
 *
 * Everything on the page is parameterized by one thing: the target. It is either
 * an instance that exists, a draft (content staged with nowhere to put it yet),
 * or nothing at all while browsing. Entry points do not fork the flow; they just
 * set the target.
 */

export const query = signal('');
export const kind = signal<ContentKind>('mod');
export const loader = signal('');
export const gameVersion = signal('');
export const sort = signal<ContentSort>('relevance');

export const results = signal<SearchHit[]>([]);
export const total = signal(0);
export const loading = signal(false);
export const loadingMore = signal(false);
export const searchError = signal<string | null>(null);

/** Content staged for an install that has not happened yet. */
export interface TrayItem {
  canonical_id: string;
  kind: ContentKind;
  title: string;
  icon_url?: string;
}

export const tray = signal<TrayItem[]>([]);

export const targetInstance = computed<EnrichedInstance | null>(() => {
  const r = route.value;
  const id = r.name === 'discover' || r.name === 'content' ? r.target : undefined;
  if (!id) return null;
  return (instances.value as EnrichedInstance[]).find((instance) => instance.id === id) ?? null;
});

/** Instances that can actually receive content: their version is known. */
export const contentTargets = computed<EnrichedInstance[]>(() =>
  (instances.value as EnrichedInstance[]).filter((instance) => instance.version_display.minecraft_label !== 'Unknown'),
);

/**
 * A draft is content staged while no instance is targeted: the set itself
 * implies which instance to build. It is what makes "pick five mods, then make
 * somewhere to put them" the same flow as "add one mod to what I have".
 */
export const isDraft = computed(() => targetInstance.value === null && tray.value.length > 0);

export function isStaged(canonicalId: string): boolean {
  return tray.value.some((item) => item.canonical_id === canonicalId);
}

export function stage(item: TrayItem): void {
  if (isStaged(item.canonical_id)) return;
  tray.value = [...tray.value, item];
}

export function unstage(canonicalId: string): void {
  tray.value = tray.value.filter((item) => item.canonical_id !== canonicalId);
}

export function clearTray(): void {
  tray.value = [];
}

export function traySelections(): ContentSelection[] {
  return tray.value.map((item) => ({ canonical_id: item.canonical_id, kind: item.kind }));
}

/** Mark a result as installed without refetching the page. */
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
