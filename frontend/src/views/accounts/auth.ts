import type { LaunchAuthMode } from '../../types';
import { boundedMessage, isRecord } from './api';
import type { AuthStatusRecord, LauncherAccount } from './types';

export function apiErrorMessage(value: unknown, fallback: string): string {
  if (!isRecord(value)) return fallback;
  return boundedMessage(typeof value.error === 'string' ? value.error : undefined, fallback);
}

export function logoutErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not clear Microsoft sign-in.');
}

export function authRefreshErrorMessage(value: unknown): string {
  if (isRecord(value)) {
    if (value.status === 'minecraft_auth_chain_failed') {
      return 'The Minecraft profile or ownership could not be verified during refresh. Re-verify with Microsoft if Online is needed.';
    }
    if (value.status === 'sign_in_required') {
      return 'Microsoft sign-in needs re-verification before Online can be used.';
    }
    if (value.status === 'refresh_failed') {
      return 'Microsoft sign-in could not be refreshed. Re-verify with Microsoft if Online is needed.';
    }
  }
  return apiErrorMessage(value, 'Could not refresh Microsoft sign-in.');
}

export function authProfileSyncErrorMessage(value: unknown): string {
  if (isRecord(value)) {
    if (value.status === 'minecraft_account_required') {
      return 'Minecraft profile sync needs a current account. Sign in or refresh the account, then try again.';
    }
    if (value.status === 'minecraft_auth_chain_failed') {
      return 'Minecraft profile sync could not verify profile or ownership. Refresh credentials or re-verify if Online is needed.';
    }
  }
  return apiErrorMessage(value, 'Could not sync Minecraft profile.');
}

export function configErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not save launch mode.');
}

export function launchAuthMode(value: unknown): LaunchAuthMode {
  return value === 'online' ? 'online' : 'offline';
}

export function statusCanSelectOnline(status: AuthStatusRecord): boolean {
  return status.online_action?.enabled === true;
}

export function accountHasLaunchReadyMinecraft(account: LauncherAccount): boolean {
  return account.online_action?.state_id === 'online_ready';
}

export function accountCanSelectOnline(account: LauncherAccount): boolean {
  return account.online_action?.enabled === true;
}
