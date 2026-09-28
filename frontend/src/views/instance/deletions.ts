import { signal } from '@preact/signals';
import { api, isApiError } from '../../api';
import { addInstance, removeInstance, updateInstanceInList } from '../../actions';
import { dtoArray, dtoEnum, dtoString, dtoWithoutError } from '../../dto-contract';
import { enrichedInstanceResponse } from '../../dto-core';
import type { DeletionSnapshot } from '../../generated/DeletionSnapshot';
import type { DeleteIntent } from '../../generated/DeleteIntent';
import { instances } from '../../store';
import { errMessage } from '../../utils';
import { clearModProvenance } from './mod-provenance-cache';

type DeletionIdentity = Omit<DeletionSnapshot, 'status'>;
export type DeletionObservation = DeletionIdentity & {
  status: DeletionSnapshot['status'] | 'unknown';
  busy: boolean;
  error: string | null;
};

// Retain observed operation identity until an authoritative terminal result is read.
export const instanceDeletions = signal<DeletionObservation[]>([]);
export const deletionDiscoveryError = signal<string | null>(null);
let revision = 0;
let discovery: Promise<void> | null = null;

function snapshot(value: unknown): DeletionSnapshot {
  const row = dtoWithoutError(value, 'Instance removal');
  const operation_id = dtoString(row.operation_id, 'Removal operation');
  const instance_id = dtoString(row.instance_id, 'Removal instance');
  if (!operation_id || !instance_id) throw new Error('Instance removal identity was invalid.');
  return {
    operation_id,
    instance_id,
    intent: dtoEnum(row.intent, 'Removal intent', ['keep_files', 'delete_files']),
    status: dtoEnum(row.status, 'Removal status', ['pending_restore', 'cleanup_pending', 'removed', 'aborted']),
  };
}

function exactSnapshot(value: unknown, expected: DeletionIdentity): DeletionSnapshot {
  const result = snapshot(value);
  if (
    result.operation_id !== expected.operation_id ||
    result.instance_id !== expected.instance_id ||
    result.intent !== expected.intent
  ) {
    throw new Error('The removal response did not match the requested instance, operation, and file choice.');
  }
  return result;
}

function remember(
  value: DeletionIdentity & { status: DeletionObservation['status'] },
  busy = false,
  error: string | null = null,
): void {
  revision += 1;
  instanceDeletions.value = [
    ...instanceDeletions.value.filter((row) => row.operation_id !== value.operation_id),
    { ...value, busy, error },
  ];
}

function forget(operationId: string): void {
  revision += 1;
  instanceDeletions.value = instanceDeletions.value.filter((row) => row.operation_id !== operationId);
}

export function pendingInstanceDeletion(instanceId: string): DeletionObservation | undefined {
  return instanceDeletions.value.find((row) => row.instance_id === instanceId);
}

export async function refreshPendingInstanceDeletions(): Promise<void> {
  if (discovery) return discovery;
  const capturedRevision = revision;
  discovery = (async () => {
    try {
      const result = dtoWithoutError(await api('GET', '/instances/pending'), 'Pending instance operations');
      const pending = dtoArray(result.deletions, 'Pending instance removals').map(snapshot);
      if (pending.some((row) => row.status !== 'pending_restore' && row.status !== 'cleanup_pending')) {
        throw new Error('Pending instance removals contained a terminal operation.');
      }
      // A list read begun before a command completed must not resurrect its old pending status.
      if (revision === capturedRevision) {
        for (const row of pending) {
          if (!instanceDeletions.value.some((known) => known.operation_id === row.operation_id)) remember(row);
        }
      }
      deletionDiscoveryError.value = null;
    } catch (error) {
      deletionDiscoveryError.value = `Could not check unfinished instance removals: ${errMessage(error)}`;
      throw error;
    }
  })();
  try {
    await discovery;
  } finally {
    discovery = null;
  }
}

async function reconcile(result: DeletionSnapshot): Promise<DeletionSnapshot> {
  remember(result);
  if (result.status === 'removed') {
    removeInstance(result.instance_id);
    clearModProvenance(result.instance_id);
    forget(result.operation_id);
  } else if (result.status === 'aborted') {
    try {
      const restored = enrichedInstanceResponse(
        await api('GET', `/instances/${encodeURIComponent(result.instance_id)}`),
      );
      if (restored.id !== result.instance_id) throw new Error('The restored instance response did not match.');
      if (instances.value.some((row) => row.id === restored.id)) updateInstanceInList(restored);
      else addInstance(restored);
      forget(result.operation_id);
    } catch (error) {
      remember(
        result,
        false,
        `The instance was preserved, but its library entry could not be refreshed: ${errMessage(error)}`,
      );
    }
  }
  return result;
}

async function readStatus(identity: DeletionIdentity): Promise<DeletionSnapshot> {
  return exactSnapshot(await api('GET', `/instances/deletions/${encodeURIComponent(identity.operation_id)}`), identity);
}

export async function checkInstanceDeletion(observation: DeletionObservation): Promise<DeletionSnapshot> {
  if (instanceDeletions.value.find((row) => row.operation_id === observation.operation_id)?.busy) {
    throw new Error('Wait for the current removal action to finish.');
  }
  remember(observation, true);
  try {
    return await reconcile(await readStatus(observation));
  } catch (error) {
    remember(
      observation,
      false,
      `The removal result is still unconfirmed. No removal was retried. ${errMessage(error)}`,
    );
    throw error;
  }
}

async function submitDeletion(identity: DeletionIdentity): Promise<DeletionSnapshot> {
  remember({ ...identity, status: 'unknown' }, true);
  try {
    const query = new URLSearchParams({
      operation_id: identity.operation_id,
      keep_files: String(identity.intent === 'keep_files'),
    });
    const response = dtoWithoutError(
      await api('DELETE', `/instances/${encodeURIComponent(identity.instance_id)}?${query}`),
      'Instance removal',
    );
    const result = exactSnapshot(response.deletion, identity);
    if (response.status !== result.status) throw new Error('The removal response contained conflicting statuses.');
    return await reconcile(result);
  } catch (submissionError) {
    // A disconnected or malformed response is not permission to repeat a destructive request.
    try {
      return await reconcile(await readStatus(identity));
    } catch (statusError) {
      const rejected =
        isApiError(submissionError) &&
        submissionError.status >= 400 &&
        submissionError.status < 500 &&
        submissionError.status !== 408;
      if (rejected && isApiError(statusError) && statusError.status === 404) {
        forget(identity.operation_id);
        throw submissionError;
      }
      const message = `The removal result could not be confirmed. Check its status in Instances before trying again. ${errMessage(submissionError)} Status check: ${errMessage(statusError)}`;
      remember({ ...identity, status: 'unknown' }, false, message);
      throw new Error(message);
    }
  }
}

export async function requestInstanceDeletion(instanceId: string, intent: DeleteIntent): Promise<DeletionSnapshot> {
  await refreshPendingInstanceDeletions();
  if (pendingInstanceDeletion(instanceId)) {
    throw new Error('This instance has an unfinished removal. Review its existing operation in Instances first.');
  }
  return submitDeletion({ operation_id: crypto.randomUUID(), instance_id: instanceId, intent });
}

/** Only an explicitly confirmed, freshly read pending operation may be replayed. */
export async function continueInstanceDeletion(result: DeletionSnapshot): Promise<DeletionSnapshot> {
  const current = instanceDeletions.value.find((row) => row.operation_id === result.operation_id);
  if (
    !current ||
    current.busy ||
    current.status !== result.status ||
    (result.status !== 'pending_restore' && result.status !== 'cleanup_pending')
  ) {
    throw new Error('The removal status changed. Check it again before continuing.');
  }
  return submitDeletion(result);
}
