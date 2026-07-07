import { api } from './api';
import { featureFlags } from './store';
import { toast } from './toast';
import type { FlagsResponse, KnownFlagKey } from './types-flags';
import { errMessage } from './utils';

let pendingFlagsRefresh: Promise<void> | null = null;

export function refreshFlags(): Promise<void> {
  if (pendingFlagsRefresh) return pendingFlagsRefresh;

  pendingFlagsRefresh = api<FlagsResponse>('GET', '/flags')
    .then((response) => {
      featureFlags.value = response.flags;
    })
    .finally(() => {
      pendingFlagsRefresh = null;
    });

  return pendingFlagsRefresh;
}

export function ensureFlags(): Promise<void> {
  if (featureFlags.value) return Promise.resolve();
  return refreshFlags();
}

export function flagEnabled(key: KnownFlagKey): boolean {
  return featureFlags.value?.find((flag) => flag.key === key)?.enabled ?? false;
}

export async function setFlagOverride(key: string, enabled: boolean | null): Promise<void> {
  const previous = featureFlags.value;
  if (previous) {
    featureFlags.value = previous.map((flag) =>
      flag.key === key
        ? {
            ...flag,
            enabled: enabled ?? flag.default_enabled,
            source: enabled === null ? 'default' : 'override',
          }
        : flag,
    );
  }

  try {
    const response = await api<FlagsResponse>('PUT', `/flags/${encodeURIComponent(key)}`, { enabled });
    featureFlags.value = response.flags;
  } catch (err) {
    featureFlags.value = previous;
    toast(errMessage(err), 'error');
  }
}
