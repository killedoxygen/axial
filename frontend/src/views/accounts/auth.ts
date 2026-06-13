import type { LaunchAuthMode } from '../../types';
import { boundedMessage, isRecord } from './api';
import type { AuthAccount, AuthStatusRecord, LauncherAccount } from './types';

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
  if (status.online_mode_ready) return true;
  return status.minecraft_profile_ready === true &&
    status.minecraft_ownership_verified === true &&
    typeof status.minecraft_token_expires_in === 'number' &&
    status.minecraft_token_expires_in > 0;
}

export function accountHasLaunchReadyMinecraft(account: AuthAccount | LauncherAccount): boolean {
  return account.minecraft_profile_ready === true &&
    account.minecraft_ownership_verified === true &&
    typeof account.minecraft_token_expires_in === 'number' &&
    account.minecraft_token_expires_in > 0;
}

export function accountCanSelectOnline(account: AuthAccount | LauncherAccount): boolean {
  return accountHasLaunchReadyMinecraft(account) || account.msa_refresh_available === true;
}
