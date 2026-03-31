import { signal } from '@preact/signals';

export type Route =
  | { page: 'launcher' }
  | { page: 'settings' }
  | { page: 'accounts' }
  | { page: 'skins' };

export const route = signal<Route>({ page: 'launcher' });

export function navigate(page: Route['page']): void {
  route.value = { page };
}
