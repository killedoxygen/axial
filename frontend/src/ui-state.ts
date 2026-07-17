import { signal } from '@preact/signals';
import type { ContentKind } from './types-content';

export type Route =
  | { name: 'home' }
  | { name: 'instances' }
  | { name: 'instance'; id: string }
  | { name: 'discover'; target?: string }
  | { name: 'content'; id: string; target?: string }
  | { name: 'dev-lab' }
  | { name: 'downloads' }
  | { name: 'accounts' }
  | { name: 'settings' };

export const ROUTE_STORAGE_KEY = 'axial:route';

export const route = signal<Route>({ name: 'home' });

const routeBackStack: Route[] = [];
const routeForwardStack: Route[] = [];

function sameRoute(a: Route, b: Route): boolean {
  return routeScrollKey(a) === routeScrollKey(b);
}

export const VIEW_SCROLL_MEMORY_LIMIT = 64;
export const VIEW_SCROLL_RESTORE_TIMEOUT_MS = 5_000;

export interface ViewScrollMemory {
  get(key: string): number | undefined;
  set(key: string, top: number): void;
  delete(key: string): void;
  readonly size: number;
}

export function routeScrollKey(r: Route): string {
  switch (r.name) {
    case 'instance':
      return JSON.stringify([r.name, r.id]);
    case 'discover':
      return JSON.stringify([r.name, r.target ?? null]);
    case 'content':
      return JSON.stringify([r.name, r.id, r.target ?? null]);
    default:
      return JSON.stringify([r.name]);
  }
}

export function routeSupportsViewScroll(r: Route): boolean {
  return r.name === 'home' || r.name === 'discover' || r.name === 'downloads';
}

export function createViewScrollMemory(maxEntries = VIEW_SCROLL_MEMORY_LIMIT): ViewScrollMemory {
  const limit = Number.isFinite(maxEntries) ? Math.max(1, Math.floor(maxEntries)) : VIEW_SCROLL_MEMORY_LIMIT;
  const positions = new Map<string, number>();

  return {
    get(key) {
      const top = positions.get(key);
      if (top === undefined) return undefined;
      positions.delete(key);
      positions.set(key, top);
      return top;
    },
    set(key, top) {
      positions.delete(key);
      if (!Number.isFinite(top) || top <= 0) return;
      positions.set(key, top);
      while (positions.size > limit) {
        const oldest = positions.keys().next().value;
        if (oldest === undefined) break;
        positions.delete(oldest);
      }
    },
    delete(key) {
      positions.delete(key);
    },
    get size() {
      return positions.size;
    },
  };
}

const viewScrollMemory = createViewScrollMemory();

interface PendingViewScrollRestore {
  key: string;
  stop: () => void;
}

let pendingViewScrollRestore: PendingViewScrollRestore | null = null;

function viewElement(): HTMLElement | null {
  if (typeof document === 'undefined') return null;
  return document.querySelector<HTMLElement>('.cp-view');
}

function cancelViewScrollRestore(): void {
  pendingViewScrollRestore?.stop();
}

function rememberViewScroll(): void {
  if (!routeSupportsViewScroll(route.value)) return;
  const key = routeScrollKey(route.value);
  if (pendingViewScrollRestore?.key === key) return;
  const view = viewElement();
  if (view) viewScrollMemory.set(key, view.scrollTop);
}

/** Immediately applies a route's last position before the next browser paint. */
export function prepareViewScroll(key: string): void {
  if (routeScrollKey(route.value) !== key) return;
  const view = viewElement();
  if (view) view.scrollTop = routeSupportsViewScroll(route.value) ? (viewScrollMemory.get(key) ?? 0) : 0;
}

/**
 * Restores a mounted route, retrying as async content grows. The returned
 * cleanup only stops this restore, so an old route cannot cancel a newer one.
 */
export function restoreViewScroll(key: string, timeoutMs = VIEW_SCROLL_RESTORE_TIMEOUT_MS): () => void {
  cancelViewScrollRestore();
  const view = viewElement();
  if (!view || routeScrollKey(route.value) !== key || !routeSupportsViewScroll(route.value)) return () => undefined;

  const top = viewScrollMemory.get(key) ?? 0;
  view.scrollTop = top;
  if (top === 0 || Math.abs(view.scrollTop - top) <= 1) return () => undefined;

  let stopped = false;
  let frame: number | null = null;
  let deadline: ReturnType<typeof setTimeout> | null = null;
  let resizeObserver: ResizeObserver | null = null;
  let mutationObserver: MutationObserver | null = null;

  const stopForPointer = (event: PointerEvent): void => {
    if (event.target === view) stop();
  };
  const stopForKey = (event: KeyboardEvent): void => {
    if ([' ', 'ArrowDown', 'ArrowUp', 'End', 'Home', 'PageDown', 'PageUp', 'Tab'].includes(event.key)) stop();
  };

  const stop = (): void => {
    if (stopped) return;
    stopped = true;
    if (frame !== null && typeof cancelAnimationFrame === 'function') cancelAnimationFrame(frame);
    if (deadline !== null) clearTimeout(deadline);
    resizeObserver?.disconnect();
    mutationObserver?.disconnect();
    view.removeEventListener('wheel', stop);
    view.removeEventListener('pointerdown', stopForPointer);
    view.removeEventListener('touchmove', stop);
    view.removeEventListener('keydown', stopForKey, true);
    view.removeEventListener('focusin', stop);
    if (pendingViewScrollRestore?.stop === stop) pendingViewScrollRestore = null;
  };

  const attempt = (): void => {
    frame = null;
    if (stopped || routeScrollKey(route.value) !== key) {
      stop();
      return;
    }
    view.scrollTop = top;
    if (Math.abs(view.scrollTop - top) <= 1) stop();
  };

  const scheduleAttempt = (): void => {
    if (stopped || frame !== null) return;
    if (typeof requestAnimationFrame === 'function') {
      frame = requestAnimationFrame(attempt);
    } else {
      attempt();
    }
  };

  if (typeof ResizeObserver !== 'undefined') {
    resizeObserver = new ResizeObserver(scheduleAttempt);
    resizeObserver.observe(view.firstElementChild ?? view);
  }
  if (typeof MutationObserver !== 'undefined') {
    mutationObserver = new MutationObserver(scheduleAttempt);
    mutationObserver.observe(view, { childList: true, subtree: true });
  }
  view.addEventListener('wheel', stop, { passive: true });
  view.addEventListener('pointerdown', stopForPointer, { passive: true });
  view.addEventListener('touchmove', stop, { passive: true });
  view.addEventListener('keydown', stopForKey, true);
  view.addEventListener('focusin', stop);

  pendingViewScrollRestore = { key, stop };
  const boundedTimeout = Number.isFinite(timeoutMs) ? Math.max(0, timeoutMs) : VIEW_SCROLL_RESTORE_TIMEOUT_MS;
  deadline = setTimeout(stop, boundedTimeout);
  return stop;
}

export function resetViewScroll(): void {
  cancelViewScrollRestore();
  viewScrollMemory.delete(routeScrollKey(route.value));
  const view = viewElement();
  if (view) view.scrollTop = 0;
}

function setRoute(r: Route): void {
  cancelViewScrollRestore();
  route.value = r;
  try {
    localStorage.setItem(ROUTE_STORAGE_KEY, JSON.stringify(r));
  } catch {}
}

export function navigate(r: Route): void {
  if (sameRoute(route.value, r)) return;
  rememberViewScroll();
  routeBackStack.push(route.value);
  routeForwardStack.length = 0;
  setRoute(r);
}

export function goBack(): void {
  const previous = routeBackStack.pop();
  if (!previous) return;
  rememberViewScroll();
  routeForwardStack.push(route.value);
  setRoute(previous);
}

export function goForward(): void {
  const next = routeForwardStack.pop();
  if (!next) return;
  rememberViewScroll();
  routeBackStack.push(route.value);
  setRoute(next);
}

function isRoute(value: unknown): value is Route {
  if (!value || typeof value !== 'object') return false;
  const candidate = value as Partial<Route>;
  const target = (candidate as { target?: unknown }).target;
  const targetOk = target === undefined || typeof target === 'string';
  switch (candidate.name) {
    case 'home':
    case 'instances':
    case 'dev-lab':
    case 'downloads':
    case 'accounts':
    case 'settings':
      return true;
    case 'discover':
      return targetOk;
    case 'content':
      return typeof (candidate as { id?: unknown }).id === 'string' && targetOk;
    case 'instance':
      return typeof (candidate as { id?: unknown }).id === 'string';
    default:
      return false;
  }
}

export function restoreRoute(): void {
  try {
    const raw = localStorage.getItem(ROUTE_STORAGE_KEY);
    if (!raw) return;
    const parsed = JSON.parse(raw) as unknown;
    if (isRoute(parsed)) setRoute(parsed);
  } catch {}
}

export const commandPaletteOpen = signal(false);
export const showOnboardingOverlay = signal(false);

export const createOpen = signal(false);
export const accountSwitcherOpen = signal(false);

export interface AccountSwitcherAnchor {
  x: number;
  y: number;
}

export const accountSwitcherAnchor = signal<AccountSwitcherAnchor | null>(null);

export interface CreateDraftItem {
  canonical_id: string;
  kind: ContentKind;
  title: string;
  icon_url?: string;
  version_id?: string;
  version_label?: string;
}

export const createDraft = signal<CreateDraftItem[] | null>(null);

export interface CreateModpackDraft {
  canonical_id: string;
  version_id: string;
  name: string;
  minecraft: string;
  loader?: string;
  loader_label: string;
  selection_id: string;
  icon_url?: string;
}

export const createModpack = signal<CreateModpackDraft | null>(null);

export function openCreate(): void {
  createDraft.value = null;
  createModpack.value = null;
  createOpen.value = true;
}

export function openCreateDraft(draft: CreateDraftItem[]): void {
  createDraft.value = draft.length > 0 ? draft : null;
  createModpack.value = null;
  createOpen.value = true;
}

export function openCreateModpack(pack: CreateModpackDraft): void {
  createDraft.value = null;
  createModpack.value = pack;
  createOpen.value = true;
}

export function closeCreate(): void {
  createOpen.value = false;
  createDraft.value = null;
  createModpack.value = null;
}

export function openAccountSwitcher(anchor?: AccountSwitcherAnchor): void {
  accountSwitcherAnchor.value = anchor ?? null;
  accountSwitcherOpen.value = true;
}

export function expandAccountSwitcher(): void {
  accountSwitcherAnchor.value = null;
}

export function closeAccountSwitcher(): void {
  accountSwitcherOpen.value = false;
  accountSwitcherAnchor.value = null;
}
