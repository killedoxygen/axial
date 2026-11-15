import { signal } from '@preact/signals';
import { errMessage, toast } from './toast';
import { getFlags, setFlag } from '../settings/api';
import type { FlagViewModel } from '../generated/FlagViewModel';
import type { FlagsResponse } from '../generated/FlagsResponse';

export interface FeatureFlagsLoadState {
  status: 'idle' | 'loading' | 'ready' | 'error';
  error: string | null;
}

export type KnownFlagKey = 'dev.state-inspector';

export const featureFlags = signal<FlagViewModel[] | null>(null);
export const featureFlagsLoadState = signal<FeatureFlagsLoadState>({ status: 'idle', error: null });

let pendingFlagsRefresh: Promise<void> | null = null;
let pendingFlagsAction: Promise<void> = Promise.resolve();
let flagsRevision: number | null = null;

function acceptFlags(response: FlagsResponse): void {
  flagsRevision = response.revision;
  featureFlags.value = response.flags;
  featureFlagsLoadState.value = { status: 'ready', error: null };
}

async function loadFlags(): Promise<void> {
  acceptFlags(await getFlags());
}

function queueFlagsAction(action: () => Promise<void>): Promise<void> {
  const request = pendingFlagsAction.then(action);
  pendingFlagsAction = request.catch(() => undefined);
  return request;
}

export function refreshFlags(): Promise<void> {
  if (pendingFlagsRefresh) return pendingFlagsRefresh;

  const pending = queueFlagsAction(async () => {
    featureFlagsLoadState.value = { status: 'loading', error: null };
    try {
      await loadFlags();
    } catch (err) {
      featureFlagsLoadState.value = { status: 'error', error: errMessage(err) };
      throw err;
    }
  }).finally(() => {
    if (pendingFlagsRefresh === pending) pendingFlagsRefresh = null;
  });
  pendingFlagsRefresh = pending;
  return pending;
}

export function ensureFlags(): Promise<void> {
  if (featureFlags.value) return Promise.resolve();
  return refreshFlags();
}

export function flagEnabled(key: KnownFlagKey): boolean {
  return featureFlags.value?.find((flag) => flag.key === key)?.enabled ?? false;
}

export async function setFlagOverride(key: string, enabled: boolean | null): Promise<void> {
  try {
    await queueFlagsAction(async () => {
      if (flagsRevision === null) await loadFlags();
      try {
        acceptFlags(await setFlag(key, enabled, flagsRevision as number));
      } catch (err) {
        // A rejected or uncertain write is reconciled with a read. Never replay
        // a mutation merely to infer whether the previous request committed.
        flagsRevision = null;
        try {
          await loadFlags();
        } catch (refreshError) {
          featureFlagsLoadState.value = { status: 'error', error: errMessage(refreshError) };
        }
        throw err;
      }
    });
  } catch (err) {
    toast(errMessage(err), 'error');
  }
}
