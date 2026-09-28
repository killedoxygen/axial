import { api } from './api';
import { featureFlags, featureFlagsLoadState } from './store';
import { toast } from './toast';
import type { FlagsResponse, KnownFlagKey } from './types-flags';
import { errMessage } from './utils';
import { dtoArray, dtoBoolean, dtoEnum, dtoNumber, dtoRecord, dtoString } from './dto-contract';

let pendingFlagsRefresh: Promise<void> | null = null;
let pendingFlagsAction: Promise<void> = Promise.resolve();
let flagsRevision: number | null = null;

function acceptFlags(response: FlagsResponse): void {
  flagsRevision = response.revision;
  featureFlags.value = response.flags;
  featureFlagsLoadState.value = { status: 'ready', error: null };
}

async function loadFlags(): Promise<void> {
  acceptFlags(flagsResponse(await api('GET', '/flags')));
}

function queueFlagsAction(action: () => Promise<void>): Promise<void> {
  const request = pendingFlagsAction.then(action);
  pendingFlagsAction = request.catch(() => undefined);
  return request;
}

export function refreshFlags(options: { fresh?: boolean } = {}): Promise<void> {
  if (pendingFlagsRefresh && !options.fresh) return pendingFlagsRefresh;

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
        const response = flagsResponse(
          await api('PUT', `/flags/${encodeURIComponent(key)}`, { enabled, expected_revision: flagsRevision }),
        );
        acceptFlags(response);
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

export function flagsResponse(value: unknown): FlagsResponse {
  const record = dtoRecord(value, 'Feature flags');
  const revision = dtoNumber(record.revision, 'Feature flags revision');
  if (!Number.isSafeInteger(revision) || revision < 0) throw new Error('Feature flags revision was invalid.');
  return {
    revision,
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
