import { signal } from '@preact/signals';
import { setConfig } from '../actions';
import { api, isApiError } from '../api';
import { hasNativeDesktopRuntime, signInWithMicrosoft } from '../native';
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
import type { AccountActionState, AuthStatusRecord, AuthStatusState, LauncherAccount } from '../views/accounts/types';

export interface AccountsSnapshot {
  state: AuthStatusState;
  accounts: LauncherAccount[];
  activeAccountId: string | null;
  status: AuthStatusRecord | null;
}

export type AccountsOpKind =
  | 'select'
  | 'create-offline'
  | 'rename-offline'
  | 'remove'
  | 'refresh-auth'
  | 'sync-profile'
  | 'sign-in';

export const accountsSnapshot = signal<AccountsSnapshot>({
  state: 'loading',
  accounts: [],
  activeAccountId: null,
  status: null,
});
export const accountsOp = signal<AccountsOpKind | null>(null);
export const accountsNotice = signal<string | null>(null);

let accountsRequestId = 0;

export function activeAccount(snapshot = accountsSnapshot.value): LauncherAccount | null {
  return snapshot.accounts.find((account) => account.active) ?? null;
}

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
  return hasNativeDesktopRuntime() || snapshot.status?.login_available !== false;
}

export async function refreshAccountsData(): Promise<void> {
  const requestId = ++accountsRequestId;
  const [accountsResult, statusResult] = await Promise.allSettled([
    api('GET', '/accounts'),
    api('GET', '/auth/status'),
  ]);
  if (requestId !== accountsRequestId) return;

  const parsedAccounts = accountsResult.status === 'fulfilled' ? launcherAccountsResponse(accountsResult.value) : null;
  const parsedStatus = statusResult.status === 'fulfilled' ? parseAuthStatus(statusResult.value) : null;
  accountsSnapshot.value = parsedAccounts
    ? {
        state: 'ready',
        accounts: parsedAccounts.accounts,
        activeAccountId: parsedAccounts.active_account_id,
        status: parsedStatus,
      }
    : { state: 'unavailable', accounts: [], activeAccountId: null, status: parsedStatus };
}

function parseAuthStatus(value: unknown): AuthStatusRecord | null {
  if (isRecord(value) && typeof value.error === 'string') return null;
  return authStatusResponse(value);
}

async function afterAccountsChange(): Promise<void> {
  try {
    setConfig(await api('GET', '/config'));
  } catch (err: unknown) {
    console.warn('Could not refresh config after account change.', err);
  }
  await refreshAccountsData();
  refreshAccountSkin();
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

async function runAccountsOp(
  kind: AccountsOpKind,
  task: () => Promise<string | null>,
  fallbackError: string,
): Promise<boolean> {
  if (accountsOp.value) return false;
  accountsOp.value = kind;
  accountsNotice.value = null;
  let succeeded = false;
  try {
    const summary = await task();
    succeeded = true;
    if (summary) toast(summary);
  } catch (err: unknown) {
    accountsNotice.value = accountsErrorText(err, fallbackError);
  } finally {
    accountsOp.value = null;
  }
  await afterAccountsChange();
  return succeeded;
}

export async function selectAccount(account: LauncherAccount): Promise<void> {
  if (accountsOp.value || account.active) return;
  if (account.kind === 'microsoft' && !actionEnabled(account.online_action)) {
    accountsNotice.value = actionUnavailableMessage(
      account.online_action,
      'This Microsoft account is not available for Online mode.',
    );
    return;
  }
  await runAccountsOp(
    'select',
    async () => {
      const response = await api('POST', `/accounts/${encodeURIComponent(account.account_id)}/select`);
      const error = commandErrorText(response);
      if (error) throw new Error(configErrorMessage(response));
      return commandSummary(response, 'Account selected.');
    },
    'Could not reach the local backend to switch account.',
  );
}

export async function createOfflineIdentity(): Promise<void> {
  if (accountsOp.value) return;
  const username = await promptNewPlayerName();
  if (!username) return;
  await runAccountsOp(
    'create-offline',
    async () => {
      const response = await api('POST', '/accounts/offline', { username });
      const error = commandErrorText(response);
      if (error) throw new Error(configErrorMessage(response));
      return commandSummary(response, 'Offline identity created.');
    },
    'Could not reach the local backend to create offline identity.',
  );
}

export async function renameOfflineIdentity(account: LauncherAccount): Promise<void> {
  if (accountsOp.value) return;
  const username = await promptPlayerName(account.display_name);
  if (!username) return;
  await runAccountsOp(
    'rename-offline',
    async () => {
      const response = await api('PATCH', `/accounts/${encodeURIComponent(account.account_id)}`, { username });
      const error = commandErrorText(response);
      if (error) throw new Error(configErrorMessage(response));
      return commandSummary(response, 'Offline identity updated.');
    },
    'Could not reach the local backend to rename offline identity.',
  );
}

export async function removeAccount(account: LauncherAccount): Promise<void> {
  if (accountsOp.value) return;
  const actionText = account.kind === 'microsoft' && account.active ? 'Sign out' : 'Remove';
  const ok = await showConfirm(`${actionText} ${account.display_name} from this launcher?`, {
    title:
      account.kind === 'microsoft' ? (account.active ? 'Sign out' : 'Remove Microsoft account') : 'Remove identity',
    destructive: true,
    confirmText: actionText,
  });
  if (!ok) return;
  await runAccountsOp(
    'remove',
    async () => {
      const response = await api('DELETE', `/accounts/${encodeURIComponent(account.account_id)}`);
      const error = commandErrorText(response);
      if (error) throw new Error(logoutErrorMessage(response));
      return commandSummary(response, 'Account removed.');
    },
    'Could not reach the local backend to remove account.',
  );
}

export async function refreshMicrosoftAuth(): Promise<void> {
  const refreshAction = activeMicrosoftRefreshAction();
  await runAccountsOp(
    'refresh-auth',
    async () => {
      const response = await api('POST', '/auth/refresh');
      const error = commandErrorText(response);
      if (error) throw new Error(authRefreshErrorMessage(response));
      if (!isRecord(response)) throw new Error('Microsoft sign-in refresh returned an invalid response.');
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
      if (!isRecord(response)) throw new Error('Minecraft profile sync returned an invalid response.');
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

export async function signInWithMicrosoftAccount(): Promise<void> {
  if (!microsoftSignInAvailable()) return;
  await runAccountsOp(
    'sign-in',
    async () => {
      const result = await signInWithMicrosoft();
      if (!result) throw new Error('Microsoft sign-in is available in the desktop app.');
      if (result.status === 'cancelled') return null;
      if (result.status !== 'authenticated') throw new Error('Microsoft sign-in returned an unexpected response.');
      return await adoptSignedInAccount(
        typeof result.login_id === 'string' && result.login_id.trim() ? result.login_id.trim() : null,
      );
    },
    'Microsoft sign-in could not be completed.',
  );
}

async function adoptSignedInAccount(loginId: string | null): Promise<string> {
  const latest = launcherAccountsResponse(await api('GET', '/accounts'));
  if (!latest) throw new Error('Account list could not be read after Microsoft sign-in.');

  const signedIn = loginId
    ? (latest.accounts.find((account) => account.kind === 'microsoft' && account.login_id === loginId) ?? null)
    : (latest.accounts.find((account) => account.active && account.kind === 'microsoft') ?? null);

  let active = signedIn?.active ? signedIn : loginId ? null : signedIn;
  if (signedIn && !signedIn.active) {
    const selected = await api('POST', `/accounts/${encodeURIComponent(signedIn.account_id)}/select`);
    const error = commandErrorText(selected);
    if (error) throw new Error(configErrorMessage(selected));
    const selectedLatest = launcherAccountsResponse(await api('GET', '/accounts'));
    active =
      selectedLatest?.accounts.find(
        (account) => account.kind === 'microsoft' && account.login_id === signedIn.login_id && account.active,
      ) ?? null;
  }

  if (!active || active.online_action?.state_id !== 'online_ready') {
    throw new Error(
      actionUnavailableMessage(active?.online_action, 'Microsoft sign-in completed, but account state is unavailable.'),
    );
  }
  try {
    await api('POST', '/skins/from-profile', { mark_current: true });
  } catch (err: unknown) {
    console.warn('Could not seed profile skin after Microsoft sign-in.', err);
  }
  return actionSuccessMessage(active.online_action, 'Account state updated.');
}
