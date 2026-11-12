import { signal } from '@preact/signals';
import type { LauncherAccount } from '../views/accounts/types';

export interface AccountsSnapshot {
  state: 'loading' | 'ready' | 'unavailable';
  accounts: LauncherAccount[];
  /** Whether this build can open the Microsoft sign-in window. */
  signInAvailable: boolean;
  revision: number | null;
  selection_revision: number | null;
}

// Read once at startup and after every account command. Kept apart from the commands so `player-skin` can
// observe it without importing them back.
export const accountsSnapshot = signal<AccountsSnapshot>({
  state: 'loading',
  accounts: [],
  signInAvailable: false,
  revision: null,
  selection_revision: null,
});

export function activeAccount(snapshot = accountsSnapshot.value): LauncherAccount | null {
  return snapshot.accounts.find((account) => account.active) ?? null;
}
