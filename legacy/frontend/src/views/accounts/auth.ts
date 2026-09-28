import { boundedMessage, isRecord } from './api';

export function apiErrorMessage(value: unknown, fallback: string): string {
  if (!isRecord(value)) return fallback;
  return boundedMessage(typeof value.error === 'string' ? value.error : undefined, fallback);
}

export function logoutErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not clear Microsoft sign-in.');
}

export function authRefreshErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not refresh Microsoft sign-in.');
}

export function authProfileSyncErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not sync Minecraft profile.');
}

export function configErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not save launch mode.');
}
