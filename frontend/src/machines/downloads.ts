import { batch, signal } from '@preact/signals';
import { api, apiUrl } from '../api';
import { showError, errMessage } from '../utils';
import { toast } from '../toast';
import { connectLoaderInstallSSE } from '../loaders/api';
import {
  hasNativeDesktopRuntime,
  nativeInstallEventName,
  nativeLoaderInstallEventName,
  onNativeEvent,
  startNativeInstallEvents,
  startNativeLoaderInstallEvents,
} from '../native';
import { instances, lastInstanceId, versions } from '../store';
import { markContentChanged } from '../content-activity';
import {
  cloneInstallItem,
  installItemFromQueueInstallItem,
  installItemFromQueuedViewModel,
  installQueueRequestFromItem,
  isSameInstallItem,
} from '../install-item';
import {
  installFailureViewModel,
  installQueueNoticePresentation,
  installProgressViewModel,
  unresolvedFailureViewModel,
} from './download-view-models';
import type {
  InstallFailureViewModel,
  InstallItem,
  InstallProgressStepViewModel,
  InstallProgressViewModel,
  InstallQueueActiveViewModel,
  InstallQueueNoticeViewModel,
  InstallQueueStateResponse,
  InstallQueueViewModel,
  InstallQueuedItemViewModel,
  InstallStatusResponse,
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

export const activeDownload = signal<ActiveDownload | null>(null);
export const downloadQueue = signal<DownloadQueue>(emptyDownloadQueue);
export const downloadFailure = signal<DownloadFailure | null>(null);

type CloseableSource = {
  close(): void;
};

type PendingInstallEventSource = CloseableSource & {
  setSource(source: CloseableSource): boolean;
};

type InstallProgressEvent = {
  phase?: string;
  current?: number;
  total?: number;
  file?: string;
  error?: string;
  done?: boolean;
  view_model?: InstallProgressViewModel;
};

let progressStream: CloseableSource | null = null;
let connectedInstallId: string | null = null;
let installOwnershipRevision = 0;
const INSTALL_STREAM_SILENCE_MS = 15_000;
const INSTALL_RECONCILE_ATTEMPTS = 4;
const INSTALL_RECONCILE_DELAY_MS = 160;

export type InstallStatusReconciliation = 'active' | 'resolved' | 'stale' | 'unavailable';
export type InstallRecoveryStrength = 'silence' | 'replace';

type InstallOperationClaim = {
  installId: string;
  operationId: string | null;
  queueId: string;
  revision: number;
  source: CloseableSource | null;
};

type InstallRecoveryEntry = {
  generation: number;
  owner: unknown;
  strength: InstallRecoveryStrength;
  completion: Promise<void>;
};

export function createInstallRecoveryCoordinator(): {
  run(
    installId: string,
    owner: unknown,
    strength: InstallRecoveryStrength,
    work: (isCurrent: () => boolean) => Promise<void>,
  ): Promise<void>;
} {
  const active = new Map<string, InstallRecoveryEntry>();
  let nextGeneration = 0;

  return {
    run(installId, owner, strength, work): Promise<void> {
      const existing = active.get(installId);
      if (existing && existing.owner === owner && (existing.strength === 'replace' || strength === 'silence')) {
        return existing.completion;
      }

      nextGeneration += 1;
      const generation = nextGeneration;
      const entry: InstallRecoveryEntry = {
        generation,
        owner,
        strength,
        completion: Promise.resolve(),
      };
      active.set(installId, entry);
      const isCurrent = (): boolean => active.get(installId)?.generation === generation;
      entry.completion = work(isCurrent).finally(() => {
        if (isCurrent()) active.delete(installId);
      });
      return entry.completion;
    },
  };
}

const installRecoveryCoordinator = createInstallRecoveryCoordinator();

export async function awaitOwnedInstallValue<T>(
  load: () => Promise<T>,
  isCurrent: () => boolean,
): Promise<{ current: true; value: T } | { current: false }> {
  if (!isCurrent()) return { current: false };
  const value = await load();
  if (!isCurrent()) return { current: false };
  return { current: true, value };
}

export async function applyInstallStreamRecovery(
  reconciliation: InstallStatusReconciliation,
  actions: {
    preserveActiveSource: boolean;
    closeSource(): void;
    restart(): Promise<void>;
  },
): Promise<void> {
  if (
    reconciliation === 'resolved' ||
    reconciliation === 'stale' ||
    (reconciliation === 'active' && actions.preserveActiveSource)
  )
    return;
  actions.closeSource();
  await actions.restart();
}

export function terminalInstallReconciliationNeedsRefresh(reconciliation: InstallStatusReconciliation): boolean {
  return reconciliation === 'active' || reconciliation === 'unavailable';
}

function setProgressStream(source: CloseableSource | null): void {
  progressStream?.close();
  progressStream = source;
}

function sameInstallOwnership(first: ActiveDownload | null, second: ActiveDownload | null): boolean {
  if (!first || !second) return first === second;
  return (
    first.queueId === second.queueId &&
    first.installId === second.installId &&
    first.operationId === second.operationId &&
    first.kind === second.kind &&
    isSameInstallItem(first.item, second.item)
  );
}

function setActiveDownload(next: ActiveDownload | null): void {
  if (!sameInstallOwnership(activeDownload.value, next)) installOwnershipRevision += 1;
  activeDownload.value = next;
}

function captureInstallOperation(
  installId: string,
  item: InstallItem,
  source: CloseableSource | null = null,
): InstallOperationClaim | null {
  const active = activeDownload.value;
  if (
    !active ||
    active.installId !== installId ||
    !isSameInstallItem(active.item, item) ||
    (source !== null && (progressStream !== source || connectedInstallId !== installId))
  ) {
    return null;
  }
  return {
    installId,
    operationId: active.operationId ?? null,
    queueId: active.queueId,
    revision: installOwnershipRevision,
    source,
  };
}

function installOperationIsCurrent(claim: InstallOperationClaim, requireSource: boolean): boolean {
  const active = activeDownload.value;
  return (
    installOwnershipRevision === claim.revision &&
    active?.installId === claim.installId &&
    active.queueId === claim.queueId &&
    (active.operationId ?? null) === claim.operationId &&
    (!requireSource ||
      (claim.source !== null && progressStream === claim.source && connectedInstallId === claim.installId))
  );
}

export function isActiveInstallItem(item: InstallItem): boolean {
  const active = activeDownload.value;
  return active !== null && isSameInstallItem(active.item, item);
}

function isActiveInstallSource(installId: string, item: InstallItem, source: CloseableSource | null): boolean {
  return (
    connectedInstallId === installId &&
    activeDownload.value?.installId === installId &&
    isActiveInstallItem(item) &&
    source !== null &&
    progressStream === source
  );
}

function activeDownloadFromQueue(active: InstallQueueActiveViewModel): ActiveDownload {
  const item = installItemFromQueueInstallItem(active.install_item);
  const startedAt =
    typeof active.install_started_at_ms === 'number' && Number.isFinite(active.install_started_at_ms)
      ? active.install_started_at_ms
      : null;
  return {
    queueId: active.queue_id,
    installId: active.install_id ?? undefined,
    operationId: active.operation_id ?? undefined,
    kind: active.kind,
    item: cloneInstallItem(item),
    displayName: active.label,
    pct: Math.max(0, Math.min(100, active.progress.progress_pct)),
    label: active.progress.label,
    phase: active.progress.phase_id,
    activeStep: active.progress.active_step ?? null,
    startedAt,
  };
}

function reconcileDownloadQueue(response: InstallQueueStateResponse): void {
  const closingStream = !response.active ? progressStream : null;
  batch(() => {
    downloadQueue.value = { items: response.items, view_model: response.view_model };
    if (response.active) {
      setActiveDownload(activeDownloadFromQueue(response.active));
      return;
    }
    if (activeDownload.value) setActiveDownload(null);
  });
  if (closingStream) {
    if (progressStream === closingStream) progressStream = null;
    closingStream.close();
  }
}

function updateActiveDownloadProgress(viewModel: InstallProgressViewModel): void {
  const current = activeDownload.value;
  if (!current) return;
  const nextPct = Number.isFinite(viewModel.progress_pct)
    ? Math.max(0, Math.min(100, viewModel.progress_pct))
    : current.pct;
  // A lower pct means a stale event: the SSE stream replays install history
  // on (re)connect after the queue snapshot already seeded current progress.
  // Keep the newer copy until replay catches up.
  const regressed = nextPct < current.pct;
  activeDownload.value = {
    ...current,
    pct: Math.max(current.pct, nextPct),
    label: regressed ? current.label : viewModel.label,
    phase: regressed ? current.phase : viewModel.phase_id || current.phase,
    activeStep: regressed ? current.activeStep : (viewModel.active_step ?? null),
  };
}

function completeActiveDownload(): void {
  setActiveDownload(null);
  setProgressStream(null);
}

export function recordDownloadFailure(
  item: InstallItem,
  displayName: string,
  viewModel: InstallFailureViewModel,
): void {
  downloadFailure.value = {
    item: cloneInstallItem(item),
    displayName,
    viewModel,
    failedAt: Date.now(),
  };
}

export function clearDownloadFailure(): void {
  downloadFailure.value = null;
}

export function clearDownloadFailureForItem(item: InstallItem): void {
  const failure = downloadFailure.value;
  if (!failure || !isSameInstallItem(failure.item, item)) return;
  downloadFailure.value = null;
}

async function installFailureViewModelForStatus(
  installId: string,
  fallback: InstallFailureViewModel,
  isCurrent: () => boolean,
): Promise<InstallFailureViewModel | null> {
  for (let attempt = 0; attempt < 4; attempt += 1) {
    if (!isCurrent()) return null;
    try {
      const ownedStatus = await awaitOwnedInstallValue(
        () => api<InstallStatusResponse>('GET', `/install/${encodeURIComponent(installId)}/status`),
        isCurrent,
      );
      if (!ownedStatus.current) return null;
      const viewModel = installFailureViewModel(ownedStatus.value.failure_view_model);
      if (viewModel) return viewModel;
    } catch {
      return isCurrent() ? fallback : null;
    }
    await delay(120);
    if (!isCurrent()) return null;
  }
  return isCurrent() ? fallback : null;
}

function showInstallQueueNotice(notice: InstallQueueNoticeViewModel | null | undefined): void {
  const presentation = installQueueNoticePresentation(notice);
  if (!presentation) return;
  toast(presentation.message, presentation.kind);
}

export async function refreshInstallQueue(
  options: { connectActive?: boolean } = {},
): Promise<InstallQueueStateResponse> {
  const response = await api<InstallQueueStateResponse>('GET', '/install/queue');
  await applyInstallQueueResponse(response, { connectActive: options.connectActive });
  return response;
}

async function refreshInstallQueueWhileCurrent(
  isCurrent: () => boolean,
  options: { connectActive?: boolean } = {},
): Promise<boolean> {
  const ownedResponse = await awaitOwnedInstallValue(
    () => api<InstallQueueStateResponse>('GET', '/install/queue'),
    isCurrent,
  );
  if (!ownedResponse.current) return false;
  await applyInstallQueueResponse(ownedResponse.value, { connectActive: options.connectActive });
  return true;
}

export async function applyInstallQueueResponse(
  response: InstallQueueStateResponse,
  options: { showNotice?: boolean; connectActive?: boolean } = {},
): Promise<InstallQueueStateResponse> {
  if (response.removed_instance_id) {
    const removedId = response.removed_instance_id;
    batch(() => {
      instances.value = instances.value.filter((instance) => instance.id !== removedId);
      if (lastInstanceId.value === removedId) lastInstanceId.value = null;
    });
  }
  const terminalActive = response.active?.progress.failed || response.active?.progress.terminal;
  reconcileDownloadQueue(response);
  if (!response.active) connectedInstallId = null;
  if (options.showNotice) showInstallQueueNotice(response.notice);
  if (terminalActive && response.active) {
    const active = response.active;
    if (!active.install_id) {
      connectedInstallId = null;
      completeActiveDownload();
      const completionRevision = installOwnershipRevision;
      if (options.connectActive) {
        await refreshInstallQueueWhileCurrent(() => installOwnershipRevision === completionRevision, {
          connectActive: true,
        });
      }
      return response;
    }
    const item = installItemFromQueueInstallItem(active.install_item);
    const source = connectedInstallId === active.install_id ? progressStream : null;
    const claim = captureInstallOperation(active.install_id, item, source);
    if (!claim) return response;
    const reconciliation = await reconcileInstallStatus(claim, item, active.label, () =>
      installOperationIsCurrent(claim, source !== null),
    );
    if (
      terminalInstallReconciliationNeedsRefresh(reconciliation) &&
      installOperationIsCurrent(claim, source !== null)
    ) {
      await refreshInstallQueueWhileCurrent(() => installOperationIsCurrent(claim, source !== null), {
        connectActive: options.connectActive,
      });
    }
  } else if (options.connectActive) {
    await connectBackendActiveInstall(response.active ?? null);
  }
  return response;
}

async function connectBackendActiveInstall(active: InstallQueueActiveViewModel | null): Promise<void> {
  if (!active?.install_id) {
    connectedInstallId = null;
    return;
  }
  if (connectedInstallId === active.install_id && activeDownload.value && progressStream) {
    return;
  }
  const item = installItemFromQueueInstallItem(active.install_item);
  connectedInstallId = active.install_id;
  const claim = captureInstallOperation(active.install_id, item);
  if (!claim) {
    connectedInstallId = null;
    return;
  }
  try {
    await connectInstallEvents(active.kind, active.install_id, item, active.label);
  } catch (err: unknown) {
    if (!installOperationIsCurrent(claim, false)) return;
    const message = errMessage(err);
    connectedInstallId = null;
    setProgressStream(null);
    showError(`Install progress connection failed: ${message}`);
    await reconcileInstallStreamIssue(active.install_id, item, active.label, {
      reconnect: false,
      notice: 'Install progress connection failed; refreshed backend status.',
    });
  }
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

async function reconcileInstallStatus(
  claim: InstallOperationClaim,
  item: InstallItem,
  displayName: string,
  isCurrent: () => boolean,
): Promise<InstallStatusReconciliation> {
  for (let attempt = 0; attempt < INSTALL_RECONCILE_ATTEMPTS; attempt += 1) {
    if (!isCurrent()) return 'stale';
    let status: InstallStatusResponse;
    try {
      const ownedStatus = await awaitOwnedInstallValue(
        () => api<InstallStatusResponse>('GET', `/install/${encodeURIComponent(claim.installId)}/status`),
        isCurrent,
      );
      if (!ownedStatus.current) return 'stale';
      status = ownedStatus.value;
    } catch {
      if (!isCurrent()) return 'stale';
      await delay(INSTALL_RECONCILE_DELAY_MS);
      if (!isCurrent()) return 'stale';
      continue;
    }
    if (!isCurrent()) return 'stale';

    const viewModel = installProgressViewModel(status.view_model);
    if (!viewModel) {
      await delay(INSTALL_RECONCILE_DELAY_MS);
      if (!isCurrent()) return 'stale';
      continue;
    }

    if (!isCurrent()) return 'stale';
    updateActiveDownloadProgress(viewModel);

    const failureViewModel = installFailureViewModel(status.failure_view_model);
    if (viewModel.failed || failureViewModel) {
      if (!isCurrent()) return 'stale';
      const failure = failureViewModel ?? unresolvedFailureViewModel(viewModel.label);
      recordDownloadFailure(item, displayName, failure);
      showError(failure.summary);
      return (await onInstallDone(undefined, isCurrent)) ? 'resolved' : 'stale';
    }

    if (status.done || viewModel.terminal) {
      if (!isCurrent()) return 'stale';
      return (await onInstallDone(item, isCurrent)) ? 'resolved' : 'stale';
    }

    return 'active';
  }

  return 'unavailable';
}

async function reconcileInstallStreamIssue(
  installId: string,
  item: InstallItem,
  displayName: string,
  options: {
    source?: CloseableSource | null;
    closeSource?: boolean;
    reconnect?: boolean;
    preserveActiveSource?: boolean;
    notice?: string;
  } = {},
): Promise<void> {
  const source = options.source ?? null;
  const claim = captureInstallOperation(installId, item, source);
  if (!claim) return;
  const recoveryOwner = source ?? `${claim.revision}:${claim.queueId}:${claim.operationId ?? ''}:${claim.installId}`;
  const strength: InstallRecoveryStrength = options.preserveActiveSource ? 'silence' : 'replace';
  await installRecoveryCoordinator.run(installId, recoveryOwner, strength, async (runnerIsCurrent) => {
    const requireSourceForResult = options.preserveActiveSource === true && source !== null;
    const closeCurrentSource = (): void => {
      if (!options.closeSource || !installOperationIsCurrent(claim, true)) return;
      connectedInstallId = null;
      setProgressStream(null);
    };
    if (!options.preserveActiveSource) closeCurrentSource();

    const reconciliation = await reconcileInstallStatus(claim, item, displayName, () => {
      return runnerIsCurrent() && installOperationIsCurrent(claim, requireSourceForResult);
    });
    if (!runnerIsCurrent() || !installOperationIsCurrent(claim, requireSourceForResult) || reconciliation === 'stale') {
      return;
    }

    await applyInstallStreamRecovery(reconciliation, {
      preserveActiveSource: options.preserveActiveSource === true,
      closeSource: closeCurrentSource,
      restart: async () => {
        if (!runnerIsCurrent() || !installOperationIsCurrent(claim, false)) return;
        try {
          await refreshInstallQueueWhileCurrent(() => runnerIsCurrent() && installOperationIsCurrent(claim, false), {
            connectActive: options.reconnect !== false,
          });
        } catch (err: unknown) {
          if (!runnerIsCurrent() || !installOperationIsCurrent(claim, false)) return;
          const prefix = options.notice || 'Install progress refresh failed.';
          showError(`${prefix} ${errMessage(err)}`);
        }
      },
    });
  });
}

type InstallStreamWatchdog = {
  markProgress(): void;
  stop(): void;
};

function createInstallStreamWatchdog(onSilence: () => Promise<void>): InstallStreamWatchdog {
  let timer: number | undefined;
  let stopped = false;
  let reconciling = false;

  const arm = (): void => {
    if (timer !== undefined) window.clearTimeout(timer);
    if (stopped) return;
    timer = window.setTimeout(() => {
      if (stopped || reconciling) {
        arm();
        return;
      }
      reconciling = true;
      void onSilence()
        .catch(() => {})
        .finally(() => {
          reconciling = false;
          arm();
        });
    }, INSTALL_STREAM_SILENCE_MS);
  };

  arm();

  return {
    markProgress: arm,
    stop(): void {
      stopped = true;
      if (timer !== undefined) window.clearTimeout(timer);
      timer = undefined;
    },
  };
}

function createPendingInstallEventSource(): PendingInstallEventSource {
  let closed = false;
  let source: CloseableSource | null = null;

  return {
    close(): void {
      if (closed) return;
      closed = true;
      source?.close();
      source = null;
    },
    setSource(nextSource: CloseableSource): boolean {
      if (closed) {
        nextSource.close();
        return false;
      }
      source = nextSource;
      return true;
    },
  };
}

async function connectNativeInstallEventSource(
  installId: string,
  item: InstallItem,
  controller: PendingInstallEventSource,
  eventName: string,
  onData: (data: InstallProgressEvent) => void,
  startEvents: () => Promise<boolean>,
  unavailableMessage: string,
): Promise<void> {
  setProgressStream(controller);

  let subscription: CloseableSource | null = null;
  try {
    subscription = await onNativeEvent(eventName, onData);
  } catch (err: unknown) {
    if (!isActiveInstallSource(installId, item, controller)) return;
    throw err;
  }

  if (!subscription) {
    if (!isActiveInstallSource(installId, item, controller)) return;
    throw new Error(unavailableMessage);
  }
  if (!controller.setSource(subscription)) return;
  if (!isActiveInstallSource(installId, item, controller)) return;

  let started = false;
  try {
    started = await startEvents();
  } catch (err: unknown) {
    if (!isActiveInstallSource(installId, item, controller)) return;
    controller.close();
    if (progressStream === controller) progressStream = null;
    throw err;
  }

  if (!started) {
    controller.close();
    if (progressStream === controller) progressStream = null;
    throw new Error(unavailableMessage);
  }
}

const STREAM_COPY = {
  vanilla: {
    unavailable: 'Desktop install progress is unavailable.',
    invalid: 'Install progress data was invalid; refreshed backend status.',
    stopped: 'Install progress stopped unexpectedly; refreshed backend status.',
  },
  loader: {
    unavailable: 'Desktop loader install progress is unavailable.',
    invalid: 'Loader install progress data was invalid. Refreshed backend status.',
    stopped: 'Loader install progress stopped unexpectedly. Refreshed backend status.',
  },
  content: {
    unavailable: 'Desktop content progress is unavailable.',
    invalid: 'Content progress data was invalid. Refreshed backend status.',
    stopped: 'Content progress stopped unexpectedly. Refreshed backend status.',
  },
} as const;

async function connectInstallEvents(
  kind: 'vanilla' | 'loader' | 'content',
  installId: string,
  item: InstallItem,
  displayName: string,
): Promise<void> {
  if (!captureInstallOperation(installId, item)) return;
  const copy = STREAM_COPY[kind];
  const nativeDesktop = hasNativeDesktopRuntime();

  let source: CloseableSource | null = null;
  let watchdog: InstallStreamWatchdog | null = null;

  const stopWatchdog = (): void => {
    watchdog?.stop();
    watchdog = null;
  };

  const startWatchdog = (): void => {
    stopWatchdog();
    watchdog = createInstallStreamWatchdog(async () => {
      if (!isActiveInstallSource(installId, item, source)) {
        stopWatchdog();
        return;
      }
      await reconcileInstallStreamIssue(installId, item, displayName, {
        source,
        closeSource: true,
        reconnect: true,
        preserveActiveSource: nativeDesktop,
      });
    });
  };

  const reconcileIssue = (notice: string): void => {
    if (!isActiveInstallSource(installId, item, source)) return;
    stopWatchdog();
    void reconcileInstallStreamIssue(installId, item, displayName, {
      source,
      closeSource: true,
      reconnect: true,
      notice,
    });
  };

  const onProgress = async (data: InstallProgressEvent): Promise<void> => {
    const claim = captureInstallOperation(installId, item, source);
    if (!claim) return;
    watchdog?.markProgress();

    const viewModel = installProgressViewModel(data.view_model);
    if (!viewModel) {
      reconcileIssue(copy.invalid);
      return;
    }

    if (!installOperationIsCurrent(claim, true)) return;
    updateActiveDownloadProgress(viewModel);
    if (viewModel.failed) {
      stopWatchdog();
      const failure = await installFailureViewModelForStatus(
        installId,
        unresolvedFailureViewModel(viewModel.label),
        () => installOperationIsCurrent(claim, true),
      );
      if (!failure || !installOperationIsCurrent(claim, true)) return;
      recordDownloadFailure(item, displayName, failure);
      showError(failure.summary);
      await onInstallDone(undefined, () => installOperationIsCurrent(claim, true));
      return;
    }
    if (data.done || viewModel.terminal) {
      stopWatchdog();
      await onInstallDone(item, () => installOperationIsCurrent(claim, true));
    }
  };

  if (nativeDesktop) {
    const controller = createPendingInstallEventSource();
    source = controller;
    startWatchdog();

    try {
      await connectNativeInstallEventSource(
        installId,
        item,
        controller,
        kind === 'loader' ? nativeLoaderInstallEventName(installId) : nativeInstallEventName(installId),
        (data) => {
          void onProgress(data);
        },
        () => (kind === 'loader' ? startNativeLoaderInstallEvents(installId) : startNativeInstallEvents(installId)),
        copy.unavailable,
      );
    } catch (err: unknown) {
      stopWatchdog();
      throw err;
    }
    return;
  }

  if (kind === 'loader') {
    const es = connectLoaderInstallSSE(
      installId,
      (data) => {
        void onProgress(data as InstallProgressEvent);
      },
      (message) => {
        reconcileIssue(`${message} Refreshed backend status.`);
      },
    );
    source = es;
    setProgressStream(es);
    startWatchdog();
    return;
  }

  const es = new EventSource(apiUrl(`/install/${installId}/events`));
  source = es;
  setProgressStream(es);
  startWatchdog();

  es.addEventListener('progress', (e: MessageEvent) => {
    let data: InstallProgressEvent;
    try {
      data = JSON.parse(e.data) as InstallProgressEvent;
    } catch {
      reconcileIssue(copy.invalid);
      return;
    }
    void onProgress(data);
  });

  es.onerror = () => {
    if (es.readyState !== EventSource.CLOSED) return;
    reconcileIssue(copy.stopped);
  };
}

export function handleInstallClick(item: InstallItem): void {
  void enqueueBackendInstallItem(item).catch(showInstallQueueError);
}

export function installVersion(target: string): void {
  if (!target) return;
  const item: InstallItem = { versionId: target };
  void enqueueBackendInstallItem(item).catch(showInstallQueueError);
}

export function retryFailedInstall(): void {
  const failure = downloadFailure.value;
  const retryAction = failure?.viewModel.retry_action;
  if (!failure || retryAction?.enabled === false) return;
  void enqueueBackendInstallItem(failure.item, { retry: true }).catch(showInstallQueueError);
}

export async function removeQueuedInstall(queueId: string): Promise<void> {
  if (!queueId) return;
  try {
    const response = await api<InstallQueueStateResponse>('DELETE', `/install/queue/${encodeURIComponent(queueId)}`);
    await applyInstallQueueResponse(response, { showNotice: true, connectActive: true });
  } catch (err: unknown) {
    showError(`Install queue update failed: ${errMessage(err)}`);
  }
}

function installQueueResponseContainsItem(response: InstallQueueStateResponse, item: InstallItem): boolean {
  const activeItem = response.active ? installItemFromQueueInstallItem(response.active.install_item) : null;
  if (activeItem && isSameInstallItem(activeItem, item)) return true;
  return response.items.some((queuedItem) => isSameInstallItem(installItemFromQueuedViewModel(queuedItem), item));
}

async function enqueueBackendInstallItem(
  item: InstallItem,
  options: { retry?: boolean } = {},
): Promise<InstallQueueStateResponse> {
  const response = await api<InstallQueueStateResponse>(
    'POST',
    options.retry ? '/install/queue/retry' : '/install/queue',
    installQueueRequestFromItem(item),
  );
  await applyInstallQueueResponse(response, { showNotice: true, connectActive: true });
  if (installQueueResponseContainsItem(response, item)) clearDownloadFailureForItem(item);
  return response;
}

function showInstallQueueError(err: unknown): void {
  showError(`Install queue failed: ${errMessage(err)}`);
}

async function onInstallDone(completedItem: InstallItem | undefined, isCurrent: () => boolean): Promise<boolean> {
  if (!isCurrent()) return false;
  connectedInstallId = null;
  completeActiveDownload();
  const completionRevision = installOwnershipRevision;
  if (completedItem) clearDownloadFailureForItem(completedItem);
  if (completedItem?.content) markContentChanged();

  try {
    const [versionsRes, instancesRes] = await Promise.all([api('GET', '/versions'), api('GET', '/instances')]);
    if (installOwnershipRevision !== completionRevision) return true;
    if (versionsRes.error) throw new Error(versionsRes.error);
    if (instancesRes.error) throw new Error(instancesRes.error);
    const nextVersions = versionsRes.versions || [];
    versions.value = nextVersions;
    instances.value = instancesRes.instances || [];
    lastInstanceId.value = instancesRes.last_instance_id || null;
  } catch (err: unknown) {
    if (installOwnershipRevision !== completionRevision) return true;
    showError(`Install completed, but failed to refresh launcher state: ${errMessage(err)}`);
  }

  if (installOwnershipRevision !== completionRevision) return true;
  try {
    await refreshInstallQueueWhileCurrent(() => installOwnershipRevision === completionRevision, {
      connectActive: true,
    });
  } catch (err: unknown) {
    if (installOwnershipRevision !== completionRevision) return true;
    showError(`Install queue refresh failed: ${errMessage(err)}`);
  }
  return true;
}
