import { signal } from '@preact/signals';
import { setConfig } from '../actions';
import { api, isApiError } from '../api';
import { signInWithMicrosoft, type NativeMicrosoftSignInResult } from '../native';
import { promptNewPlayerName, promptPlayerName } from '../player-name';
import { refreshAccountSkin } from '../player-skin';
import { toast } from '../toast';
import { showConfirm } from '../ui/Dialog';
import {
  authStatusResponse,
  boundedMessage,
  commandSummary,
  isRecord,
  launcherAccountsResponse,
} from '../views/accounts/api';
import {
  authProfileSyncErrorMessage,
  authRefreshErrorMessage,
  configErrorMessage,
  logoutErrorMessage,
} from '../views/accounts/auth';
import type { AccountActionState, AuthStatusRecord, LauncherAccount } from '../views/accounts/types';
import { configResponse } from '../dto-core';
import { refreshInstanceReadiness } from '../instance-readiness';
import { accountsSnapshot, activeAccount, type AccountsSnapshot } from './accounts-state';
export { accountsSnapshot, activeAccount, type AccountsSnapshot } from './accounts-state';

export type AccountsOpKind =
  | 'select'
  | 'create-offline'
  | 'rename-offline'
  | 'remove'
  | 'refresh-auth'
  | 'sync-profile'
  | 'sign-in';

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

export function actionSuccessMessage(action: AccountActionState | undefined, fallback: string): string {
  return action?.success_summary || action?.label || fallback;
}

export function microsoftSignInAvailable(snapshot = accountsSnapshot.value): boolean {
  return snapshot.state === 'ready' && snapshot.status?.login_available === true;
}

export function refreshAccountsData(options: { fresh?: boolean } = {}): Promise<void> {
  if (options.fresh) invalidateAccountsRead();
  if (accountsRefresh) return accountsRefresh;
  const requestId = ++accountsRequestId;
  const pending = readAccountsData(requestId).finally(() => {
    if (accountsRefresh === pending) accountsRefresh = null;
  });
  accountsRefresh = pending;
  return pending;
}

async function readAccountsData(requestId: number): Promise<void> {
  // A selection may change between these two reads. Rebase once without replaying a mutation.
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const [accountsResult, statusResult] = await Promise.allSettled([
      api('GET', '/accounts'),
      api('GET', '/auth/status'),
    ]);
    if (requestId !== accountsRequestId) return;
    const directory = accountsResult.status === 'fulfilled' ? launcherAccountsResponse(accountsResult.value) : null;
    const status = statusResult.status === 'fulfilled' ? parseAuthStatus(statusResult.value) : null;
    if (directory && status && directory.selection_revision === status.selection_revision &&
        directory.launch_auth_mode === status.launch_auth_mode) {
      const previousRevision = accountsSnapshot.value.revision;
      if (previousRevision !== null && directory.revision < previousRevision) continue;
      accountsSnapshot.value = {
        state: 'ready',
        accounts: directory.accounts,
        status,
        revision: directory.revision,
        selection_revision: directory.selection_revision,
      };
      refreshAccountSkin();
      return;
    }
  }
  accountsSnapshot.value = {
    state: 'unavailable', accounts: [], status: null,
    revision: accountsSnapshot.value.revision,
    selection_revision: null,
  };
  refreshAccountSkin();
}

function invalidateAccountsRead(): void {
  accountsRequestId += 1;
  accountsRefresh = null;
}

function parseAuthStatus(value: unknown): AuthStatusRecord | null {
  if (isRecord(value) && typeof value.error === 'string') return null;
  return authStatusResponse(value);
}

async function afterAccountsChange(): Promise<void> {
  try {
    setConfig(configResponse(await api('GET', '/config')));
  } catch (err: unknown) {
    console.warn('Could not refresh config after account change.', err);
  }
  await refreshAccountsData();
  await refreshInstanceReadiness();
}

function accountsErrorText(error: unknown, fallback: string): string {
  if (isApiError(error)) return boundedMessage(apiPayloadError(error.payload), fallback);
  if (error instanceof Error) return boundedMessage(error.message, fallback);
  return fallback;
}

function apiPayloadError(payload: unknown): string | undefined {
  return isRecord(payload) && typeof payload.error === 'string' ? payload.error : undefined;
}

function commandErrorText(response: unknown): string | null {
  return isRecord(response) && typeof response.error === 'string' ? response.error : null;
}

function requireCommand(response: unknown, status: string, accountId?: string): void {
  const error = commandErrorText(response);
  if (error) throw new Error(error);
  if (!isRecord(response) || response.status !== status) {
    throw new Error('The backend returned an invalid account response. Refresh accounts before trying again.');
  }
  if (accountId && response.account_id !== accountId &&
      (!isRecord(response.account) || response.account.account_id !== accountId)) {
    throw new Error('The backend returned a different account. Refresh accounts before trying again.');
  }
}

function selectionFence(): { expected_selection_revision: number } {
  const snapshot = accountsSnapshot.value;
  if (snapshot.state !== 'ready' || snapshot.selection_revision === null) {
    throw new Error('Accounts are unavailable. Refresh accounts before trying again.');
  }
  return { expected_selection_revision: snapshot.selection_revision };
}

function accountFence(account: LauncherAccount): {
  expected_account_revision: number;
  expected_selection_revision: number;
} {
  const selection = selectionFence();
  const current = accountsSnapshot.value.accounts.find((item) => item.account_id === account.account_id);
  if (!current || current.account_revision !== account.account_revision) {
    throw new Error('This account changed. Refresh accounts before trying again.');
  }
  return { ...selection, expected_account_revision: account.account_revision };
}

async function runAccountsOp(
  kind: AccountsOpKind,
  task: () => Promise<string | null>,
  fallbackError: string,
  verify?: (snapshot: AccountsSnapshot) => boolean,
): Promise<boolean> {
  if (accountsOp.value) return false;
  accountsOp.value = kind;
  accountsNotice.value = null;
  let succeeded = false;
  try {
    const summary = await task();
    invalidateAccountsRead();
    await afterAccountsChange();
    if (summary && (accountsSnapshot.value.state !== 'ready' || (verify && !verify(accountsSnapshot.value)))) {
      throw new Error('The account request finished, but its current state could not be read. Refresh accounts.');
    }
    succeeded = summary !== null;
    if (summary) toast(summary);
  } catch (err: unknown) {
    accountsNotice.value = accountsErrorText(err, fallbackError);
    invalidateAccountsRead();
    await afterAccountsChange();
  } finally {
    accountsOp.value = null;
  }
  return succeeded;
}

export async function selectAccount(account: LauncherAccount): Promise<boolean> {
  if (accountsOp.value) return false;
  if (account.active) return activeAccount()?.account_id === account.account_id;
  if (account.kind === 'microsoft' && !actionEnabled(account.online_action)) {
    accountsNotice.value = actionUnavailableMessage(
      account.online_action,
      'This Microsoft account is not available for Online mode.',
    );
    return false;
  }
  return runAccountsOp(
    'select',
    async () => {
      const response = await api('POST', `/accounts/${encodeURIComponent(account.account_id)}/select`, accountFence(account));
      const error = commandErrorText(response);
      if (error) throw new Error(configErrorMessage(response));
      requireCommand(response, 'account_selected', account.account_id);
      return commandSummary(response, 'Account selected.');
    },
    'Could not reach the local backend to switch account.',
    (snapshot) => activeAccount(snapshot)?.account_id === account.account_id,
  );
}

export async function createOfflineIdentity(): Promise<void> {
  let accountId: string | null = null;
  await runAccountsOp(
    'create-offline',
    async () => {
      const fence = selectionFence();
      const username = await promptNewPlayerName();
      if (!username) return null;
      const created = await createOffline(username, fence);
      accountId = created.accountId;
      return created.summary;
    },
    'Could not reach the local backend to create offline identity.',
    (snapshot) => snapshot.accounts.some((account) => account.account_id === accountId && account.kind === 'offline'),
  );
}

export async function createOfflineAccount(username: string): Promise<boolean> {
  let accountId: string | null = null;
  return runAccountsOp('create-offline', async () => {
    const created = await createOffline(username, selectionFence());
    accountId = created.accountId;
    return created.summary;
  }, 'Could not reach the local backend to create offline identity.',
  (snapshot) => snapshot.accounts.some((account) => account.account_id === accountId && account.kind === 'offline'));
}

async function createOffline(username: string, fence: ReturnType<typeof selectionFence>): Promise<{
  accountId: string;
  summary: string;
}> {
  const response = await api('POST', '/accounts/offline', { username, ...fence });
  requireCommand(response, 'account_created');
  if (!isRecord(response) || !isRecord(response.account) || typeof response.account.account_id !== 'string') {
    throw new Error('The backend did not return the created account.');
  }
  return { accountId: response.account.account_id, summary: commandSummary(response, 'Offline identity created.') };
}

export async function renameOfflineIdentity(account: LauncherAccount): Promise<void> {
  if (accountsOp.value) return;
  let requestedName: string | null = null;
  let renamedAccountId: string | null = null;
  await runAccountsOp(
    'rename-offline',
    async () => {
      const fence = accountFence(account);
      const username = await promptPlayerName(account.display_name);
      if (!username) return null;
      requestedName = username;
      const renamed = await renameOfflineAccount(account, username, fence);
      renamedAccountId = renamed.accountId;
      return renamed.summary;
    },
    'Could not reach the local backend to rename offline identity.',
    (snapshot) => snapshot.accounts.some((item) => item.account_id === renamedAccountId && item.display_name === requestedName),
  );
}

/** Commits a name drafted outside the switcher through the shared account owner. */
export async function saveOfflineIdentityName(
  account: LauncherAccount,
  username: string,
  successMessage = 'Offline identity updated.',
): Promise<boolean> {
  if (account.kind !== 'offline') return false;
  let renamedAccountId: string | null = null;
  return runAccountsOp(
    'rename-offline',
    async () => {
      const renamed = await renameOfflineAccount(account, username, accountFence(account), successMessage);
      renamedAccountId = renamed.accountId;
      return renamed.summary;
    },
    'Could not reach the local backend to rename offline identity.',
    (snapshot) => snapshot.accounts.some((item) => item.account_id === renamedAccountId && item.display_name === username),
  );
}

async function renameOfflineAccount(
  account: LauncherAccount,
  username: string,
  fence: ReturnType<typeof accountFence>,
  successMessage = 'Offline identity updated.',
): Promise<{ accountId: string; summary: string }> {
  const response = await api('PATCH', `/accounts/${encodeURIComponent(account.account_id)}`, { username, ...fence });
  const error = commandErrorText(response);
  if (error) throw new Error(configErrorMessage(response));
  requireCommand(response, 'account_updated');
  if (!isRecord(response) || !isRecord(response.account) ||
      typeof response.account.account_id !== 'string' || !response.account.account_id ||
      response.account.kind !== 'offline' || response.account.display_name !== username) {
    throw new Error('The backend did not return the renamed offline identity.');
  }
  return { accountId: response.account.account_id, summary: commandSummary(response, successMessage) };
}

export async function removeAccount(account: LauncherAccount): Promise<void> {
  if (accountsOp.value) return;
  const actionText = account.kind === 'microsoft' && account.active ? 'Sign out' : 'Remove';
  await runAccountsOp(
    'remove',
    async () => {
      const fence = accountFence(account);
      const ok = await showConfirm(`${actionText} ${account.display_name} from this launcher?`, {
        title: account.kind === 'microsoft' ? (account.active ? 'Sign out' : 'Remove Microsoft account') : 'Remove identity',
        destructive: true,
        confirmText: actionText,
      });
      if (!ok) return null;
      const query = new URLSearchParams({
        expected_account_revision: String(fence.expected_account_revision),
        expected_selection_revision: String(fence.expected_selection_revision),
      });
      const response = await api('DELETE', `/accounts/${encodeURIComponent(account.account_id)}?${query}`);
      const error = commandErrorText(response);
      if (error) throw new Error(logoutErrorMessage(response));
      requireCommand(response, 'account_removed', account.account_id);
      return commandSummary(response, 'Account removed.');
    },
    'Could not reach the local backend to remove account.',
    (snapshot) => !snapshot.accounts.some((item) => item.account_id === account.account_id),
  );
}

export async function refreshMicrosoftAuth(): Promise<void> {
  const refreshAction = activeMicrosoftRefreshAction();
  if (!actionEnabled(refreshAction)) return;
  await runAccountsOp(
    'refresh-auth',
    async () => {
      const response = await api('POST', '/auth/refresh');
      const error = commandErrorText(response);
      if (error) throw new Error(authRefreshErrorMessage(response));
      requireCommand(response, 'refreshed');
      return commandSummary(response, actionSuccessMessage(refreshAction, 'Account state updated.'));
    },
    'Could not reach the local backend to refresh Microsoft sign-in.',
  );
}

export async function syncMinecraftProfile(): Promise<void> {
  const syncAction = activeMicrosoftProfileSyncAction();
  if (!actionEnabled(syncAction)) return;
  await runAccountsOp(
    'sync-profile',
    async () => {
      const response = await api('POST', '/auth/profile/sync');
      const error = commandErrorText(response);
      if (error) throw new Error(authProfileSyncErrorMessage(response));
      requireCommand(response, 'profile_synced');
      return commandSummary(response, actionSuccessMessage(syncAction, 'Account state updated.'));
    },
    'Could not reach the local backend to sync Minecraft profile.',
  );
}

export function activeMicrosoftRefreshAction(): AccountActionState | undefined {
  const snapshot = accountsSnapshot.value;
  const active = activeAccount(snapshot);
  return (active?.kind === 'microsoft' ? active.refresh_action : undefined) ?? snapshot.status?.refresh_action;
}

export function activeMicrosoftProfileSyncAction(): AccountActionState | undefined {
  const snapshot = accountsSnapshot.value;
  const active = activeAccount(snapshot);
  return (
    (active?.kind === 'microsoft' ? active.profile_sync_action : undefined) ?? snapshot.status?.profile_sync_action
  );
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
      const summary = await adoptSignedInAccount(result.login_id);
      authenticated = result;
      return summary;
    },
    'Microsoft sign-in could not be completed.',
    (snapshot) => activeAccount(snapshot)?.login_id === authenticated?.login_id,
  );
  return succeeded ? authenticated : null;
}

async function adoptSignedInAccount(loginId: string): Promise<string> {
  const latest = launcherAccountsResponse(await api('GET', '/accounts'));
  if (!latest) throw new Error('Account list could not be read after Microsoft sign-in.');

  const signedIn = latest.accounts.find((account) => account.kind === 'microsoft' && account.login_id === loginId) ?? null;

  // Native completion already performs a backend compare-and-select. A late UI
  // completion must never select an account over a more recent user choice.
  const active = signedIn?.active ? signedIn : null;

  if (!active || active.online_action?.state_id !== 'online_ready') {
    throw new Error(
      actionUnavailableMessage(active?.online_action, 'Microsoft sign-in completed, but account state is unavailable.'),
    );
  }
  return actionSuccessMessage(active.online_action, 'Account state updated.');
}
