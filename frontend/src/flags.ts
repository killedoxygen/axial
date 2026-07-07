import { api } from './api';
import { featureFlags } from './store';
import { toast } from './toast';
import type { FlagsResponse, KnownFlagKey } from './types-flags';
import { errMessage } from './utils';

export async function refreshFlags(): Promise<void> {
  const response = await api<FlagsResponse>('GET', '/flags');
  featureFlags.value = response.flags;
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
