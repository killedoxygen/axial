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

// The shell observes this same snapshot without eagerly loading account commands.
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
