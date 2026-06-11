import { signal } from '@preact/signals';

export type Route =
  | { name: 'home' }
  | { name: 'instances' }
  | { name: 'instance'; id: string }
  | { name: 'dev-lab' }
  | { name: 'downloads' }
  | { name: 'accounts' }
  | { name: 'settings' };

export const ROUTE_STORAGE_KEY = 'croopor:route';

export const route = signal<Route>({ name: 'home' });

const routeBackStack: Route[] = [];
const routeForwardStack: Route[] = [];

function sameRoute(a: Route, b: Route): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

function setRoute(r: Route): void {
  route.value = r;
  try { localStorage.setItem(ROUTE_STORAGE_KEY, JSON.stringify(r)); } catch {}
}

export function navigate(r: Route): void {
  if (sameRoute(route.value, r)) return;
  routeBackStack.push(route.value);
  routeForwardStack.length = 0;
  setRoute(r);
}

export function goBack(): void {
  const previous = routeBackStack.pop();
  if (!previous) return;
  routeForwardStack.push(route.value);
  setRoute(previous);
}

export function goForward(): void {
  const next = routeForwardStack.pop();
  if (!next) return;
  routeBackStack.push(route.value);
  setRoute(next);
}

function isRoute(value: unknown): value is Route {
  if (!value || typeof value !== 'object') return false;
  const candidate = value as Partial<Route>;
  switch (candidate.name) {
    case 'home':
    case 'instances':
    case 'dev-lab':
    case 'downloads':
    case 'accounts':
    case 'settings':
      return true;
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
export const showSetupOverlay = signal(false);

export const createOpen = signal(false);

export function openCreate(): void {
  createOpen.value = true;
}

export function closeCreate(): void {
  createOpen.value = false;
}
