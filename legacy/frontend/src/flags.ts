import { api } from './api';
import { featureFlags, featureFlagsLoadState } from './store';
import { toast } from './toast';
import type { FlagsResponse, KnownFlagKey } from './types-flags';
import { errMessage } from './utils';
import { dtoArray, dtoBoolean, dtoEnum, dtoRecord, dtoString } from './dto-contract';

let pendingFlagsRefresh: Promise<void> | null = null;

export function refreshFlags(): Promise<void> {
  if (pendingFlagsRefresh) return pendingFlagsRefresh;

  featureFlagsLoadState.value = { status: 'loading', error: null };
  pendingFlagsRefresh = api('GET', '/flags')
    .then(flagsResponse)
    .then((response) => {
      featureFlags.value = response.flags;
      featureFlagsLoadState.value = { status: 'ready', error: null };
    })
    .catch((err: unknown) => {
      featureFlagsLoadState.value = { status: 'error', error: errMessage(err) };
      throw err;
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
    const response = flagsResponse(await api('PUT', `/flags/${encodeURIComponent(key)}`, { enabled }));
    featureFlags.value = response.flags;
    featureFlagsLoadState.value = { status: 'ready', error: null };
  } catch (err) {
    featureFlags.value = previous;
    toast(errMessage(err), 'error');
  }
}

export function flagsResponse(value: unknown): FlagsResponse {
  const record = dtoRecord(value, 'Feature flags');
  return {
    flags: dtoArray(record.flags, 'Feature flags list').map((flag) => {
      const entry = dtoRecord(flag, 'Feature flag');
      return {
        key: dtoString(entry.key, 'Feature flag key'),
        title: dtoString(entry.title, 'Feature flag title'),
        description: dtoString(entry.description, 'Feature flag description'),
        stage: dtoEnum(entry.stage, 'Feature flag stage', ['experimental', 'beta'] as const),
        dev_only: dtoBoolean(entry.dev_only, 'Feature flag development scope'),
        default_enabled: dtoBoolean(entry.default_enabled, 'Feature flag default'),
        enabled: dtoBoolean(entry.enabled, 'Feature flag enabled'),
        source: dtoEnum(entry.source, 'Feature flag source', ['default', 'override'] as const),
      };
    }),
  };
}
