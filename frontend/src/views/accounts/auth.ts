import type { LaunchAuthMode } from '../../types';
import { boundedMessage, isRecord, maybeNumber, minecraftReadiness } from './api';
import type { AuthStatusRecord, MinecraftAuthReadiness } from './types';

export interface AuthLoginPending {
  status: 'pending';
  login_id: string;
  user_code: string;
  verification_uri: string;
  expires_in: number;
  interval: number;
  message?: string;
}

interface AuthPollPending {
  status: 'pending';
  interval: number;
  poll_hint?: string;
}

interface AuthPollAuthenticated {
  status: 'msa_authenticated';
  msa_provider?: string | null;
  msa_token_expires_in?: number | null;
}

export type AuthPollAuthenticatedRecord = AuthPollAuthenticated & MinecraftAuthReadiness;

type AuthPollTerminalStatus =
  | 'authorization_declined'
  | 'expired'
  | 'bad_verification_code'
  | 'minecraft_auth_chain_failed'
  | 'error';

interface AuthPollTerminal {
  status: AuthPollTerminalStatus;
  error?: string;
  auth_chain_error?: string;
  poll_hint?: string;
}

export type AuthPollResponse = AuthPollPending | AuthPollAuthenticatedRecord | AuthPollTerminal;

export function loginPendingResponse(value: unknown): AuthLoginPending | null {
  if (!isRecord(value)) return null;
  if (
    value.status !== 'pending' ||
    typeof value.login_id !== 'string' ||
    typeof value.user_code !== 'string' ||
    typeof value.verification_uri !== 'string' ||
    typeof value.expires_in !== 'number' ||
    typeof value.interval !== 'number'
  ) {
    return null;
  }

  return {
    status: 'pending',
    login_id: value.login_id,
    user_code: value.user_code,
    verification_uri: value.verification_uri,
    expires_in: value.expires_in,
    interval: value.interval,
    message: typeof value.message === 'string' ? value.message : undefined,
  };
}

export function pollResponse(value: unknown): AuthPollResponse | null {
  if (!isRecord(value)) return null;
  if (value.status === 'pending' && typeof value.interval === 'number') {
    return {
      status: 'pending',
      interval: value.interval,
      poll_hint: typeof value.poll_hint === 'string' ? value.poll_hint : undefined,
    };
  }

  if (value.status === 'msa_authenticated') {
    return {
      status: 'msa_authenticated',
      msa_provider: typeof value.msa_provider === 'string' ? value.msa_provider : undefined,
      msa_token_expires_in: value.msa_token_expires_in === null ? null : maybeNumber(value.msa_token_expires_in),
      ...minecraftReadiness(value),
    };
  }

  if (
    value.status === 'authorization_declined' ||
    value.status === 'expired' ||
    value.status === 'bad_verification_code' ||
    value.status === 'minecraft_auth_chain_failed' ||
    value.status === 'error'
  ) {
    return {
      status: value.status,
      error: typeof value.error === 'string' ? value.error : undefined,
      auth_chain_error: typeof value.auth_chain_error === 'string' ? value.auth_chain_error : undefined,
      poll_hint: typeof value.poll_hint === 'string' ? value.poll_hint : undefined,
    };
  }

  return null;
}

export function pollTerminalMessage(response: AuthPollResponse | null): string {
  if (!response || response.status === 'pending' || response.status === 'msa_authenticated') {
    return 'Microsoft sign-in returned an unexpected response.';
  }
  if (response.status === 'minecraft_auth_chain_failed') {
    return 'Microsoft sign-in completed, but the Minecraft profile or ownership could not be verified. Offline launch remains available.';
  }
  const fallback = response.status === 'authorization_declined'
    ? 'Microsoft sign-in was declined.'
    : response.status === 'expired'
      ? 'Microsoft sign-in expired. Get a new code to try again.'
      : response.status === 'bad_verification_code'
        ? 'Microsoft sign-in code was rejected. Get a new code to try again.'
        : 'Microsoft sign-in could not be completed.';
  return boundedMessage(response.error || response.poll_hint, fallback);
}

export function apiErrorMessage(value: unknown, fallback: string): string {
  if (!isRecord(value)) return fallback;
  return boundedMessage(typeof value.error === 'string' ? value.error : undefined, fallback);
}

export function pollErrorMessage(value: unknown): string {
  const response = pollResponse(value);
  if (response && response.status !== 'pending' && response.status !== 'msa_authenticated') {
    return pollTerminalMessage(response);
  }
  return apiErrorMessage(value, 'Microsoft sign-in could not be completed.');
}

export function loginErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not start Microsoft sign-in.');
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

export function configErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not save launch mode.');
}

export function pollSuccessMessage(poll: AuthPollAuthenticatedRecord): string {
  const profileName = poll.minecraft_profile?.name;
  if (poll.minecraft_profile_ready && poll.minecraft_ownership_verified) {
    return `${profileName || 'Minecraft profile'} verified. Online launch is ready while these credentials remain valid.`;
  }
  if (poll.minecraft_profile_ready) {
    return `${profileName || 'Minecraft profile'} was found, but ownership was not verified. Offline launch remains available.`;
  }
  return 'Microsoft sign-in is active, but Minecraft profile verification is not complete.';
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

export function formatSeconds(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return 'unknown';
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const minutes = Math.floor(seconds / 60);
  const remaining = Math.round(seconds % 60);
  return remaining > 0 ? `${minutes}m ${remaining}s` : `${minutes}m`;
}

export async function copyText(text: string): Promise<void> {
  if (!navigator.clipboard) {
    throw new Error('clipboard API unavailable');
  }
  await navigator.clipboard.writeText(text);
}
