import { signal } from '@preact/signals';
import type { AuthStatusRecord, AuthStatusState, LauncherAccount } from '../views/accounts/types';

export interface AccountsSnapshot {
  state: AuthStatusState;
  accounts: LauncherAccount[];
  status: AuthStatusRecord | null;
  revision: number | null;
  selection_revision: number | null;
}

// The shell observes this same snapshot without eagerly loading account commands.
export const accountsSnapshot = signal<AccountsSnapshot>({
  state: 'loading',
  accounts: [],
  status: null,
  revision: null,
  selection_revision: null,
});

export function activeAccount(snapshot = accountsSnapshot.value): LauncherAccount | null {
  return snapshot.accounts.find((account) => account.active) ?? null;
}
