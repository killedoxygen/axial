// Cross module UI state: route, overlays, command palette
// Instance data and selection live in store.ts
import { signal } from '@preact/signals';

export type Route =
  | { name: 'home' }
  | { name: 'instances' }
  | { name: 'instance'; id: string }
  | { name: 'create' }
  | { name: 'dev-lab' }
  | { name: 'browse' }
  | { name: 'downloads' }
  | { name: 'accounts' }
  | { name: 'settings' };

export const route = signal<Route>({ name: 'home' });

export function navigate(r: Route): void {
  route.value = r;
  try { localStorage.setItem('croopor:route', JSON.stringify(r)); } catch {}
}

export function restoreRoute(): void {
  try {
    const raw = localStorage.getItem('croopor:route');
    if (!raw) return;
    const parsed = JSON.parse(raw) as Route;
    if (parsed && typeof parsed.name === 'string') route.value = parsed;
  } catch {}
}

export const commandPaletteOpen = signal(false);
export const windowMaximized = signal(false);
export const showOnboardingOverlay = signal(false);
export const showSetupOverlay = signal(false);
