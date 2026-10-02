import { batch, computed, effect, signal } from '@preact/signals';
import { api } from '../api';
import { errMessage, showError } from '../utils';
import { toast } from '../toast';
import { connectInstallQueueSSE } from '../loaders/api';
import { config, instances, lastInstanceId, launchSessions, launchState, versions } from '../store';
import { markContentChanged } from '../content-activity';
import { installQueueStateResponse } from '../dto-install';
import { instancesResponse, versionsResponse } from '../dto-core';
import {
  cloneInstallItem,
  installItemFromQueueInstallItem,
  installQueueRequestFromItem,
  isSameInstallItem,
} from '../install-item';
import { installQueueNoticePresentation } from './download-view-models';
import type {
  InstallActionViewModel,
  InstallFailureViewModel,
  InstallItem,
  InstallProgressStepViewModel,
  InstallQueueActiveViewModel,
  InstallQueueStateResponse,
  InstallQueueViewModel,
  InstallQueuedItemViewModel,
} from '../types-install';

export type ActiveDownload = {
  queueId: string;
  installId?: string;
  operationId?: string;
  kind: 'vanilla' | 'loader' | 'content';
  item: InstallItem;
  displayName: string;
  pct: number;
  label: string;
  phase: string;
  activeStep: InstallProgressStepViewModel | null;
  startedAt: number | null;
  retryAction?: InstallActionViewModel;
};

export type DownloadFailure = {
  item: InstallItem;
  displayName: string;
  viewModel: InstallFailureViewModel;
  failedAt: number;
};

export type DownloadQueue = {
  items: InstallQueuedItemViewModel[];
  view_model: InstallQueueViewModel;
};

export const emptyDownloadQueue: DownloadQueue = {
  items: [],
  view_model: {
    state_id: 'idle',
    status_label: 'Idle',
    title: 'Nothing downloading',
    summary: 'Launch an instance that needs a download, or install a new Minecraft version, and it will show up here.',
    queued_count: 0,
    queued_count_label: 'No queued downloads',
    queued_item_label: 'No items queued',
    next_label: null,
    active_queued_count_label: null,
    section_title: 'Queue',
    empty_title: 'Nothing downloading',
    empty_summary:
      'Launch an instance that needs a download, or install a new Minecraft version, and it will show up here.',
  },
};

// Every view derives from the backend's full queue snapshot, including after reload.
const queueSnapshot = signal<InstallQueueStateResponse | null>(null);
const dismissedFailureOperation = signal<string | null>(null);
export const activeInstallRetryPending = signal(false);
let closeQueueStream: (() => void) | null = null;
let registryRefreshGeneration = 0;
const retiredQueueEpochs = new Set<string>();
type RegistryCursor = { epoch: string; revision: number };
let reconciledRegistry: RegistryCursor | null = null;
let registryRead: { cursor: RegistryCursor; generation: number; promise: Promise<void>; dispose: () => void } | null = null;
let registryRetryTimer: ReturnType<typeof setTimeout> | undefined;
let registryReadFailures = 0;

export const activeDownload = computed<ActiveDownload | null>(() => {
  const active = queueSnapshot.value?.active;
  return active ? activeDownloadFromQueue(active) : null;
});

export const downloadQueue = computed<DownloadQueue>(() => {
  const snapshot = queueSnapshot.value;
  return snapshot ? { items: snapshot.items, view_model: snapshot.view_model } : emptyDownloadQueue;
});

export const downloadFailure = computed<DownloadFailure | null>(() => {
  const failure = queueSnapshot.value?.latest_failure;
  if (!failure || failure.operation_id === dismissedFailureOperation.value) return null;
  return {
    item: cloneInstallItem(installItemFromQueueInstallItem(failure.install_item)),
    displayName: failure.label,
    viewModel: failure.failure_view_model,
    failedAt: failure.failed_at_ms,
  };
});

function activeDownloadFromQueue(active: InstallQueueActiveViewModel): ActiveDownload {
  return {
    queueId: active.queue_id,
    installId: active.install_id ?? undefined,
    operationId: active.operation_id ?? undefined,
    kind: active.kind,
    item: cloneInstallItem(installItemFromQueueInstallItem(active.install_item)),
    displayName: active.label,
    pct: Math.max(0, Math.min(100, active.progress.progress_pct)),
    label: active.progress.label,
    phase: active.progress.phase_id,
    activeStep: active.progress.active_step ?? null,
    startedAt: active.install_started_at_ms ?? null,
    retryAction: active.retry_action,
  };
}

export function isActiveInstallItem(item: InstallItem): boolean {
  const active = activeDownload.value;
  return active !== null && isSameInstallItem(active.item, item);
}

export function clearDownloadFailure(): void {
  const failure = queueSnapshot.value?.latest_failure;
  const action = failure?.failure_view_model.dismiss_action;
  if (!failure || action?.action !== 'dismiss' || action.enabled !== true) return;
  // This local notice dismissal applies only to the captured operation.
  dismissedFailureOperation.value = failure.operation_id;
}

function ensureQueueSubscription(): void {
  if (closeQueueStream) return;
  closeQueueStream = connectInstallQueueSSE(
    (response) => {
      void applyInstallQueueResponse(response).catch(showInstallQueueError);
    },
    (error) => {
      showError('Install progress connection failed: ' + errMessage(error));
    },
  );
}

export function disconnectInstallQueue(): void {
  closeQueueStream?.();
  closeQueueStream = null;
  registryRefreshGeneration += 1;
  registryRead?.dispose();
  if (registryRetryTimer !== undefined) clearTimeout(registryRetryTimer);
  registryRetryTimer = undefined;
}

export async function refreshInstallQueue(
  options: { connectActive?: boolean; requireInstalledState?: boolean } = {},
): Promise<InstallQueueStateResponse> {
  if (options.requireInstalledState) {
    // Each explicit startup attempt needs a fresh projection: account/config or
    // instance changes need not advance the install queue's registry revision.
    registryRefreshGeneration += 1;
    registryRead?.dispose();
    reconciledRegistry = null;
    if (registryRetryTimer !== undefined) clearTimeout(registryRetryTimer);
    registryRetryTimer = undefined;
  }
  // Subscribe while idle too: the next operation may originate elsewhere.
  // Each connection begins with an atomic complete backend snapshot.
  if (options.connectActive) ensureQueueSubscription();
  const epochAtRequest = queueSnapshot.value?.queue_epoch;
  const response = installQueueStateResponse(await api('GET', '/install/queue'));
  const currentEpoch = queueSnapshot.value?.queue_epoch;
  if (!currentEpoch || currentEpoch === epochAtRequest || currentEpoch === response.queue_epoch) {
    await applyInstallQueueResponse(response, { requireInstalledState: options.requireInstalledState });
  }
  // A superseded queue read is not proof that the current registry projection
  // has loaded. Startup must join the current owner's read before becoming ready.
  if (options.requireInstalledState) await refreshInstalledState(true);
  return response;
}

export async function applyInstallQueueResponse(
  response: InstallQueueStateResponse,
  options: { showNotice?: boolean; connectActive?: boolean; requireInstalledState?: boolean } = {},
): Promise<InstallQueueStateResponse> {
  if (options.connectActive) ensureQueueSubscription();
  if (!response.queue_epoch || !Number.isSafeInteger(response.registry_revision) || response.registry_revision < 0 ||
      !Number.isSafeInteger(response.revision) || response.revision < 0) {
    throw new Error('Install queue revision is invalid.');
  }
  const previous = queueSnapshot.value;
  if (retiredQueueEpochs.has(response.queue_epoch)) return response;
  if (previous?.queue_epoch === response.queue_epoch) {
    if (response.revision < previous.revision) return response;
    if (response.revision === previous.revision) {
      await refreshInstalledState(options.requireInstalledState);
      return response;
    }
    if (response.registry_revision < previous.registry_revision) {
      throw new Error('Install registry revision moved backwards.');
    }
  } else if (previous) {
    retiredQueueEpochs.add(previous.queue_epoch);
  }

  batch(() => {
    queueSnapshot.value = response;
    if (response.removed_instance_id) {
      const removedId = response.removed_instance_id;
      instances.value = instances.value.filter((instance) => instance.id !== removedId);
      if (lastInstanceId.value === removedId) lastInstanceId.value = null;
    }
  });

  if (options.showNotice) {
    const notice = installQueueNoticePresentation(response.notice);
    if (notice) toast(notice.message, notice.kind);
  }

  // A complete snapshot retains invalidation even when an entire operation was
  // coalesced before this client observed its active phase.
  if (previous?.queue_epoch !== response.queue_epoch || previous.registry_revision !== response.registry_revision) {
    markContentChanged();
    if (registryRetryTimer !== undefined) clearTimeout(registryRetryTimer);
    registryRetryTimer = undefined;
  }
  await refreshInstalledState(options.requireInstalledState);
  return response;
}

function sameRegistry(left: RegistryCursor | null, right: RegistryCursor): boolean {
  return left?.epoch === right.epoch && left.revision === right.revision;
}

async function refreshInstalledState(required = false): Promise<void> {
  for (;;) {
    const snapshot = queueSnapshot.value;
    if (!snapshot) {
      if (required) throw new Error('Install state has not loaded.');
      return;
    }
    const cursor = { epoch: snapshot.queue_epoch, revision: snapshot.registry_revision };
    if (sameRegistry(reconciledRegistry, cursor)) return;
    if (registryRetryTimer !== undefined) {
      if (required) throw new Error('Install state could not be refreshed. Retry startup.');
      return;
    }
    const read = registryRead && registryRead.generation === registryRefreshGeneration && sameRegistry(registryRead.cursor, cursor)
      ? registryRead.promise : readInstalledState(cursor);
    try {
      await read;
    } catch (error) {
      if (required) throw error;
      return;
    }
    if (!required) return;
    const current = queueSnapshot.value;
    if (!current || current.queue_epoch !== cursor.epoch || current.registry_revision !== cursor.revision) continue;
    if (!sameRegistry(reconciledRegistry, cursor)) {
      throw new Error(registryRetryTimer !== undefined
        ? 'Launcher state changed while refreshing. Retry startup.'
        : 'Install state refresh was interrupted. Retry startup.');
    }
    return;
  }
}

function readInstalledState(cursor: RegistryCursor): Promise<void> {
  const previous = registryRead?.promise;
  registryRead?.dispose();
  const generation = ++registryRefreshGeneration;
  let stopWatching = (): void => {};
  const promise = Promise.resolve().then(async () => {
    try {
      // Superseding a response does not stop its backend proof. Drain both
      // requests before the newest cursor starts another projection.
      await previous?.catch(() => {});
      // Account edits and session settlement do not advance the install cursor.
      // Rebase once against their existing owners before falling back to paced retry.
      for (let attempt = 0; attempt < 2; attempt += 1) {
        if (generation !== registryRefreshGeneration) return;
        const expectedInstances = instances.value;
        const expectedVersions = versions.value;
        const expectedConfig = config.value;
        const expectedLastInstance = lastInstanceId.value;
        const expectedSessions = Object.entries(launchSessions.value).map(([id, session]) => [id, session.sessionId] as const);
        const expectedPreparing = launchState.value.status === 'preparing' ? launchState.value.instanceId : null;
        const ownershipCurrent = (): boolean =>
          Object.keys(launchSessions.value).length === expectedSessions.length &&
          expectedSessions.every(([id, sessionId]) => launchSessions.value[id]?.sessionId === sessionId) &&
          (launchState.value.status === 'preparing' ? launchState.value.instanceId : null) === expectedPreparing;
        const inputsCurrent = (): boolean => instances.value === expectedInstances && versions.value === expectedVersions &&
          config.value === expectedConfig && lastInstanceId.value === expectedLastInstance && ownershipCurrent();
        // A session may start and end during this pair of reads. Remember that
        // transition without invalidating on ordinary same-session progress ticks.
        let superseded = false;
        stopWatching = effect(() => { if (!inputsCurrent()) superseded = true; });
        try {
          const [versionsRead, instancesRead] = await Promise.allSettled([
            api('GET', '/versions').then(versionsResponse),
            api('GET', '/instances').then(instancesResponse),
          ]);
          if (generation !== registryRefreshGeneration) return;
          if (superseded || !inputsCurrent()) continue;
          if (versionsRead.status === 'rejected') throw versionsRead.reason;
          if (instancesRead.status === 'rejected') throw instancesRead.reason;
          stopWatching();
          batch(() => {
            versions.value = versionsRead.value.versions;
            instances.value = instancesRead.value.instances;
            lastInstanceId.value = instancesRead.value.last_instance_id;
          });
          reconciledRegistry = cursor;
          registryReadFailures = 0;
          return;
        } catch (error: unknown) {
          if (generation !== registryRefreshGeneration) return;
          if (superseded || !inputsCurrent()) continue;
          if (registryReadFailures === 0) {
            showError('Install state changed, but launcher state could not be refreshed: ' + errMessage(error));
          }
          scheduleRegistryRetry(Math.min(10000, 500 * 2 ** Math.min(registryReadFailures++, 5)));
          throw error;
        } finally {
          stopWatching();
        }
      }
      scheduleRegistryRetry(500);
    } finally {
      if (registryRead?.promise === promise) registryRead = null;
    }
  });
  registryRead = { cursor, generation, promise, dispose: () => stopWatching() };
  return promise;
}

function scheduleRegistryRetry(delay: number): void {
  registryRetryTimer = setTimeout(() => {
    registryRetryTimer = undefined;
    void refreshInstalledState();
  }, delay);
}

export function handleInstallClick(item: InstallItem): void {
  void enqueueBackendInstallItem(item).catch(showInstallQueueError);
}

export function installVersion(target: string): void {
  if (target) handleInstallClick({ versionId: target });
}

export function retryFailedInstall(): void {
  const failure = downloadFailure.value;
  const action = failure?.viewModel.retry_action;
  if (!failure || action?.action !== 'retry' || action.enabled !== true) return;
  void enqueueBackendInstallItem(failure.item, { retry: true }).catch(showInstallQueueError);
}

export async function retryActiveInstall(expectedInstallId: string | undefined): Promise<void> {
  const active = activeDownload.value;
  const action = active?.retryAction;
  if (!expectedInstallId || active?.installId !== expectedInstallId || activeInstallRetryPending.value ||
      action?.action !== 'retry' || action.enabled !== true) return;
  activeInstallRetryPending.value = true;
  try {
    await enqueueBackendInstallItem(active.item, { retry: true, expectedInstallId });
  } catch (error: unknown) {
    showInstallQueueError(error);
  } finally {
    activeInstallRetryPending.value = false;
  }
}

export async function removeQueuedInstall(queueId: string): Promise<void> {
  const item = downloadQueue.value.items.find((candidate) => candidate.queue_id === queueId);
  if (!item || item.remove_action.action !== 'remove_from_queue' || item.remove_action.enabled !== true) return;
  try {
    const response = installQueueStateResponse(await api('DELETE', '/install/queue/' + encodeURIComponent(queueId)));
    await applyInstallQueueResponse(response, { showNotice: true, connectActive: true });
  } catch (error: unknown) {
    showError('Install queue update failed: ' + errMessage(error));
    await reconcileUncertainMutation();
  }
}

async function enqueueBackendInstallItem(
  item: InstallItem,
  options: { retry?: boolean; expectedInstallId?: string } = {},
): Promise<InstallQueueStateResponse> {
  const request = installQueueRequestFromItem(item);
  const path = (options.retry ? '/install/queue/retry' : '/install/queue') +
    (options.expectedInstallId ? '?expected_install_id=' + encodeURIComponent(options.expectedInstallId) : '');
  try {
    const response = installQueueStateResponse(
      await api('POST', path, request),
    );
    await applyInstallQueueResponse(response, { showNotice: true, connectActive: true });
    return response;
  } catch (error: unknown) {
    await reconcileUncertainMutation();
    throw error;
  }
}

export async function reconcileUncertainMutation(): Promise<void> {
  // A lost response is resolved by an authoritative read, never mutation replay.
  await refreshInstallQueue({ connectActive: true }).catch(showInstallQueueError);
}

function showInstallQueueError(error: unknown): void {
  showError('Install queue failed: ' + errMessage(error));
}
