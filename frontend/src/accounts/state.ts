import { signal } from '@preact/signals';
import { setConfig, config } from '../settings/state';
import { isApiError } from '../platform/transport';
import { getConfig } from '../settings/api';
import { refreshInstanceReadiness } from '../instances/readiness';
import { signInWithMicrosoft, type NativeMicrosoftSignInResult } from '../platform/native';
import { promptNewPlayerName, promptPlayerName } from './player-name';
import { forgetAccountSkin, moveAccountSkin } from './player-skin';
import { toast, showError } from '../app/toast';
import { showConfirm } from '../ui/Dialog';
import * as accountsApi from './api';
import { boundedMessage, type AccountFence } from './api';
import type { AccountActionState } from '../generated/AccountActionState';
import type { AccountCommandResponse } from '../generated/AccountCommandResponse';
import type { AccountView } from '../generated/AccountView';
import { accountsSnapshot, activeAccount, type AccountsSnapshot } from './snapshot';
export { accountsSnapshot, activeAccount, type AccountsSnapshot } from './snapshot';

export type AccountsOpKind =
  'select' | 'create-offline' | 'rename-offline' | 'remove' | 'refresh-auth' | 'sync-profile' | 'sign-in';

export const accountsOp = signal<AccountsOpKind | null>(null);
export const accountsNotice = signal<string | null>(null);

let accountsRequestId = 0;
let accountsRefresh: Promise<void> | null = null;

export function actionEnabled(action: AccountActionState | undefined): boolean {
  return action?.enabled === true;
}

export function actionUnavailableMessage(action: AccountActionState | undefined, fallback: string): string {
  return action?.disabled_reason || action?.detail || fallback;
}

function actionSuccessMessage(action: AccountActionState | undefined, fallback: string): string {
  return action?.success_summary || action?.label || fallback;
}

export function microsoftSignInAvailable(snapshot = accountsSnapshot.value): boolean {
  return snapshot.state === 'ready' && snapshot.signInAvailable;
}

export function refreshAccountsData(): Promise<void> {
  if (accountsRefresh) return accountsRefresh;
  const requestId = ++accountsRequestId;
  const pending = readAccountsData(requestId).finally(() => {
    if (accountsRefresh === pending) accountsRefresh = null;
  });
  accountsRefresh = pending;
  return pending;
}

async function readAccountsData(requestId: number): Promise<void> {
  let next: AccountsSnapshot;
  try {
    const list = await accountsApi.listAccounts();
    next = {
      state: 'ready',
      accounts: list.accounts,
      signInAvailable: list.sign_in_available,
      revision: list.revision,
      selection_revision: list.selection_revision,
    };
  } catch {
    // A read that fails takes the commands away, never the accounts already known: the shell keeps showing who
    // is playing.
    next = {
      state: 'unavailable',
      accounts: accountsSnapshot.value.accounts,
      signInAvailable: false,
      revision: accountsSnapshot.value.revision,
      selection_revision: null,
    };
  }
  // A command that finished meanwhile started a newer read.
  if (requestId !== accountsRequestId) return;
  accountsSnapshot.value = next;
}

/** What launch readiness depends on: the settings and selection revisions and the selected account's online state. */
function readinessInputs(): string {
  const snapshot = accountsSnapshot.value;
  const active = activeAccount(snapshot);
  return JSON.stringify([
    config.value?.revision,
    config.value?.account_selection_revision,
    snapshot.state,
    snapshot.revision,
    snapshot.selection_revision,
    active?.account_id,
    active?.online_action.state_id,
    active?.online_action.enabled,
  ]);
}

/**
 * Re-reads what an account command can change: the projected config, the
 * accounts and launch readiness. A command the player backed out of only
 * rechecks readiness when something else changed meanwhile.
 */
async function afterAccountsChange(outcome: 'acknowledged' | 'failed' | 'cancelled'): Promise<void> {
  const before = readinessInputs();
  accountsRequestId += 1;
  accountsRefresh = null;
  try {
    const next = await getConfig();
    const current = config.value;
    // Replacing an unchanged config would invalidate a readiness read already in flight.
    if (current?.revision !== next.revision || current.account_selection_revision !== next.account_selection_revision) {
      setConfig(next);
    }
  } catch (err: unknown) {
    console.warn('Could not refresh config after account change.', err);
    if (outcome === 'cancelled') outcome = 'failed';
  }
  await refreshAccountsData();
  if (outcome === 'cancelled' && accountsSnapshot.value.state === 'ready' && readinessInputs() === before) return;
  if (outcome !== 'acknowledged') return refreshInstanceReadiness();
  try {
    await refreshInstanceReadiness(undefined, { retry: false });
  } catch {
    showError('Could not refresh launch availability. Refresh the launcher to check again.');
  }
}

function accountsErrorText(error: unknown, fallback: string): string {
  if (isApiError(error) || error instanceof Error) return boundedMessage(error.message, fallback);
  return fallback;
}

/** The account a command acknowledged, after checking it is the command's own acknowledgement. */
function commandAccount(response: AccountCommandResponse, status: string): AccountCommandResponse['account'] {
  if (response.status !== status) {
    throw new Error('The backend returned an invalid account response. Refresh accounts before trying again.');
  }
  return response.account;
}

function selectionFence(): { expected_selection_revision: number } {
  const snapshot = accountsSnapshot.value;
  if (snapshot.state !== 'ready' || snapshot.selection_revision === null) {
    throw new Error('Accounts are unavailable. Refresh accounts before trying again.');
  }
  return { expected_selection_revision: snapshot.selection_revision };
}

function accountFence(account: AccountView): AccountFence {
  const selection = selectionFence();
  const current = accountsSnapshot.value.accounts.find((item) => item.account_id === account.account_id);
  if (!current || current.account_revision !== account.account_revision) {
    throw new Error('This account changed. Refresh accounts before trying again.');
  }
  return { ...selection, expected_account_revision: account.account_revision };
}

/**
 * Runs one account command at a time. The task returns the sentence to show, or
 * null when the player backed out before anything was sent.
 */
async function runAccountsOp(
  kind: AccountsOpKind,
  task: () => Promise<string | null>,
  fallbackError: string,
  verify?: (snapshot: AccountsSnapshot) => boolean,
): Promise<boolean> {
  if (accountsOp.value) return false;
  accountsOp.value = kind;
  accountsNotice.value = null;
  try {
    const summary = await task();
    await afterAccountsChange(summary === null ? 'cancelled' : 'acknowledged');
    if (summary === null) return false;
    if (accountsSnapshot.value.state !== 'ready' || (verify && !verify(accountsSnapshot.value))) {
      throw new Error('The account request finished, but its current state could not be read. Refresh accounts.');
    }
    toast(summary);
    return true;
  } catch (err: unknown) {
    accountsNotice.value = accountsErrorText(err, fallbackError);
    await afterAccountsChange('failed');
    return false;
  } finally {
    accountsOp.value = null;
  }
}

export async function selectAccount(account: AccountView): Promise<boolean> {
  if (accountsOp.value) return false;
  if (account.active) return activeAccount()?.account_id === account.account_id;
  if (account.kind === 'microsoft' && !actionEnabled(account.online_action)) {
    accountsNotice.value = actionUnavailableMessage(
      account.online_action,
      'This Microsoft account is not available for Online mode.',
    );
    return false;
  }
  const id = account.account_id;
  return runAccountsOp(
    'select',
    async () => {
      const response = await accountsApi.selectAccount(id, accountFence(account));
      if (commandAccount(response, 'account_selected').account_id !== id) {
        throw new Error('The backend selected a different account. Refresh accounts before trying again.');
      }
      return response.view_model.summary;
    },
    'Could not reach the local backend to switch account.',
    (snapshot) => activeAccount(snapshot)?.account_id === id,
  );
}

/** Creates and selects an offline identity; asks for its name when none is given. */
export function createOfflineAccount(username?: string): Promise<boolean> {
  let created: string | null = null;
  return runAccountsOp(
    'create-offline',
    async () => {
      const fence = selectionFence();
      const name = username ?? (await promptNewPlayerName());
      if (!name) return null;
      const response = await accountsApi.createOfflineAccount(name, fence);
      created = commandAccount(response, 'account_created').account_id;
      return response.view_model.summary;
    },
    'Could not reach the local backend to create offline identity.',
    (snapshot) => snapshot.accounts.some((account) => account.account_id === created && account.kind === 'offline'),
  );
}

/**
 * Renames an offline identity; asks for the name when none is given. The rename
 * gives the identity a new id, so its chosen look moves with it.
 */
export function renameOfflineAccount(
  account: AccountView,
  username?: string,
  successMessage = 'Offline identity updated.',
): Promise<boolean> {
  if (account.kind !== 'offline') return Promise.resolve(false);
  let renamed: { account_id: string; display_name: string } | null = null;
  return runAccountsOp(
    'rename-offline',
    async () => {
      const fence = accountFence(account);
      const name = username ?? (await promptPlayerName(account.display_name));
      if (!name) return null;
      const response = await accountsApi.renameOfflineAccount(account.account_id, name, fence);
      renamed = commandAccount(response, 'account_updated');
      if (renamed.display_name !== name) throw new Error('The backend did not return the renamed offline identity.');
      moveAccountSkin(account.account_id, renamed.account_id);
      return response.view_model.summary || successMessage;
    },
    'Could not reach the local backend to rename offline identity.',
    (snapshot) =>
      snapshot.accounts.some(
        (item) => item.account_id === renamed?.account_id && item.display_name === renamed.display_name,
      ),
  );
}

export async function removeAccount(account: AccountView): Promise<void> {
  const actionText = account.kind === 'microsoft' && account.active ? 'Sign out' : 'Remove';
  const id = account.account_id;
  await runAccountsOp(
    'remove',
    async () => {
      const fence = accountFence(account);
      const ok = await showConfirm(`${actionText} ${account.display_name} from this launcher?`, {
        title:
          account.kind === 'microsoft' ? (account.active ? 'Sign out' : 'Remove Microsoft account') : 'Remove identity',
        destructive: true,
        confirmText: actionText,
      });
      if (!ok) return null;
      const response = await accountsApi.removeAccount(id, fence);
      if (response.status !== 'account_removed' || response.account_id !== id) {
        throw new Error('The backend returned an invalid account response. Refresh accounts before trying again.');
      }
      forgetAccountSkin(id);
      return response.view_model.summary;
    },
    'Could not reach the local backend to remove account.',
    (snapshot) => !snapshot.accounts.some((item) => item.account_id === id),
  );
}

/** Runs a command on the selected Microsoft account when the backend says it is available. */
async function runActiveAccountAction(
  kind: 'refresh-auth' | 'sync-profile',
  action: AccountActionState | undefined,
  command: () => Promise<{ status: string; view_model: { summary: string } }>,
  status: string,
  fallbackError: string,
): Promise<void> {
  if (!actionEnabled(action)) return;
  await runAccountsOp(
    kind,
    async () => {
      const response = await command();
      if (response.status !== status) {
        throw new Error('The backend returned an invalid account response. Refresh accounts before trying again.');
      }
      return response.view_model.summary || actionSuccessMessage(action, 'Account state updated.');
    },
    fallbackError,
  );
}

export function refreshMicrosoftAuth(): Promise<void> {
  return runActiveAccountAction(
    'refresh-auth',
    activeMicrosoftAccount()?.refresh_action,
    accountsApi.refreshMicrosoftSignIn,
    'refreshed',
    'Could not reach the local backend to refresh Microsoft sign-in.',
  );
}

export function syncMinecraftProfile(): Promise<void> {
  return runActiveAccountAction(
    'sync-profile',
    activeMicrosoftAccount()?.profile_sync_action,
    accountsApi.syncMinecraftProfile,
    'profile_synced',
    'Could not reach the local backend to sync Minecraft profile.',
  );
}

export function activeMicrosoftAccount(): AccountView | null {
  const active = activeAccount();
  return active?.kind === 'microsoft' ? active : null;
}

export async function signInWithMicrosoftAccount(): Promise<NativeMicrosoftSignInResult | null> {
  if (!microsoftSignInAvailable()) return null;
  let authenticated: NativeMicrosoftSignInResult | null = null;
  const succeeded = await runAccountsOp(
    'sign-in',
    async () => {
      const result = await signInWithMicrosoft();
      if (!result) throw new Error('Microsoft sign-in is available in the desktop app.');
      if (result.status === 'cancelled') return null;
      if (result.status !== 'authenticated') throw new Error('Microsoft sign-in returned an unexpected response.');
      if (typeof result.login_id !== 'string' || !result.login_id.trim()) {
        throw new Error('Microsoft sign-in did not identify the authenticated account.');
      }
      const summary = await signedInSummary(result.login_id);
      authenticated = result;
      return summary;
    },
    'Microsoft sign-in could not be completed.',
    (snapshot) => activeAccount(snapshot)?.login_id === authenticated?.login_id,
  );
  return succeeded ? authenticated : null;
}

/**
 * Native completion already performs a backend compare-and-select, so this only
 * reads the result: a late completion must never select over a newer choice.
 */
async function signedInSummary(loginId: string): Promise<string> {
  const { accounts } = await accountsApi.listAccounts();
  const signedIn = accounts.find((account) => account.active && account.login_id === loginId);
  if (signedIn?.online_action.state_id !== 'online_ready') {
    throw new Error(
      actionUnavailableMessage(
        signedIn?.online_action,
        'Microsoft sign-in completed, but account state is unavailable.',
      ),
    );
  }
  return actionSuccessMessage(signedIn.online_action, 'Account state updated.');
}
