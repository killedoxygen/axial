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
import { catalog, instances, lastInstanceId, versions } from '../store';
import {
  cloneInstallItem,
  installItemFromQueueInstallItem,
  installItemFromQueuedViewModel,
  installQueueRequestFromItem,
  isSameInstallItem,
} from '../install-item';
import {
  installFailureViewModel,
  installProgressViewModel,
  queueNoticeToastKind,
  unresolvedFailureViewModel,
} from './download-view-models';
import type { LoaderBuildRecord } from '../types-loader';
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
  kind: 'vanilla' | 'loader';
  item: InstallItem;
  displayName: string;
  pct: number;
  label: string;
  phase: string;
  activeStep: InstallProgressStepViewModel | null;
  startedAt: number;
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
const INSTALL_STREAM_SILENCE_MS = 15_000;
const INSTALL_RECONCILE_ATTEMPTS = 4;
const INSTALL_RECONCILE_DELAY_MS = 160;
const reconcilingInstallIds = new Set<string>();

function setProgressStream(source: CloseableSource | null): void {
  progressStream?.close();
  progressStream = source;
}

export function isActiveInstallItem(item: InstallItem): boolean {
  const active = activeDownload.value;
  return active !== null && isSameInstallItem(active.item, item);
}

function isActiveInstallSource(item: InstallItem, source: CloseableSource | null): boolean {
  return isActiveInstallItem(item) && source !== null && progressStream === source;
}

function activeDownloadFromQueue(active: InstallQueueActiveViewModel): ActiveDownload {
  const item = installItemFromQueueInstallItem(active.install_item);
  const current = activeDownload.value;
  const startedAt =
    current && isSameInstallItem(current.item, item) && current.installId === (active.install_id ?? undefined)
      ? current.startedAt
      : Date.now();
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
      activeDownload.value = activeDownloadFromQueue(response.active);
      return;
    }
    if (activeDownload.value) activeDownload.value = null;
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
  activeDownload.value = null;
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
): Promise<InstallFailureViewModel> {
  for (let attempt = 0; attempt < 4; attempt += 1) {
    try {
      const status = await api<InstallStatusResponse>('GET', `/install/${encodeURIComponent(installId)}/status`);
      const viewModel = installFailureViewModel(status.failure_view_model);
      if (viewModel) return viewModel;
    } catch {
      return fallback;
    }
    await delay(120);
  }
  return fallback;
}

async function recordBackendInstallFailure(
  item: InstallItem,
  displayName: string,
  installId: string,
  fallback: InstallFailureViewModel,
): Promise<InstallFailureViewModel> {
  const viewModel = await installFailureViewModelForStatus(installId, fallback);
  recordDownloadFailure(item, displayName, viewModel);
  return viewModel;
}

function showInstallQueueNotice(notice: InstallQueueNoticeViewModel | null | undefined): void {
  if (!notice?.message?.trim()) return;
  const message = notice.detail?.trim() ? `${notice.message}: ${notice.detail.trim()}` : notice.message.trim();
  toast(message, queueNoticeToastKind(notice));
}

export async function refreshInstallQueue(
  options: { connectActive?: boolean; retryPendingStart?: boolean } = {},
): Promise<InstallQueueStateResponse> {
  let latestResponse: InstallQueueStateResponse | null = null;
  for (let attempt = 0; attempt < (options.retryPendingStart ? 4 : 1); attempt += 1) {
    const response = await api<InstallQueueStateResponse>('GET', '/install/queue');
    latestResponse = response;
    await applyInstallQueueResponse(response, { connectActive: options.connectActive });
    if (!options.retryPendingStart || response.active || response.items.length === 0) return response;
    await delay(120);
  }
  if (latestResponse) return latestResponse;
  throw new Error('Install queue did not return a response.');
}

export async function applyInstallQueueResponse(
  response: InstallQueueStateResponse,
  options: { showNotice?: boolean; connectActive?: boolean } = {},
): Promise<InstallQueueStateResponse> {
  const terminalActive = response.active?.progress.failed || response.active?.progress.terminal;
  const activeResponse = terminalActive ? { ...response, active: null } : response;
  reconcileDownloadQueue(activeResponse);
  if (!activeResponse.active) connectedInstallId = null;
  if (options.showNotice) showInstallQueueNotice(response.notice);
  if (terminalActive && response.active) {
    const active = response.active;
    if (!active.install_id) {
      if (options.connectActive) await refreshInstallQueue({ connectActive: true, retryPendingStart: true });
      return response;
    }
    const item = installItemFromQueueInstallItem(active.install_item);
    const resolved = await reconcileInstallStatus(active.install_id, item, active.label);
    if (!resolved) await refreshInstallQueue({ connectActive: options.connectActive, retryPendingStart: true });
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
  try {
    await connectInstallEvents(active.kind, active.install_id, item, active.label);
  } catch (err: unknown) {
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

async function reconcileInstallStatus(installId: string, item: InstallItem, displayName: string): Promise<boolean> {
  for (let attempt = 0; attempt < INSTALL_RECONCILE_ATTEMPTS; attempt += 1) {
    let status: InstallStatusResponse;
    try {
      status = await api<InstallStatusResponse>('GET', `/install/${encodeURIComponent(installId)}/status`);
    } catch {
      await delay(INSTALL_RECONCILE_DELAY_MS);
      continue;
    }

    const viewModel = installProgressViewModel(status.view_model);
    if (!viewModel) {
      await delay(INSTALL_RECONCILE_DELAY_MS);
      continue;
    }

    updateActiveDownloadProgress(viewModel);

    const failureViewModel = installFailureViewModel(status.failure_view_model);
    if (viewModel.failed || failureViewModel) {
      const failure = failureViewModel ?? unresolvedFailureViewModel(viewModel.label);
      recordDownloadFailure(item, displayName, failure);
      showError(failure.summary);
      await onInstallDone();
      return true;
    }

    if (status.done || viewModel.terminal) {
      await onInstallDone(item);
      return true;
    }

    return false;
  }

  return false;
}

async function reconcileInstallStreamIssue(
  installId: string,
  item: InstallItem,
  displayName: string,
  options: {
    source?: CloseableSource | null;
    closeSource?: boolean;
    reconnect?: boolean;
    notice?: string;
  } = {},
): Promise<void> {
  if (!isActiveInstallItem(item) || reconcilingInstallIds.has(installId)) return;
  reconcilingInstallIds.add(installId);

  try {
    if (options.closeSource && options.source && progressStream === options.source) {
      connectedInstallId = null;
      setProgressStream(null);
    }

    const resolved = await reconcileInstallStatus(installId, item, displayName);
    if (resolved || !isActiveInstallItem(item)) return;

    try {
      await refreshInstallQueue({
        connectActive: options.reconnect !== false,
        retryPendingStart: true,
      });
    } catch (err: unknown) {
      const prefix = options.notice || 'Install progress refresh failed.';
      showError(`${prefix} ${errMessage(err)}`);
    }
  } finally {
    reconcilingInstallIds.delete(installId);
  }
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
    if (!isActiveInstallSource(item, controller)) return;
    throw err;
  }

  if (!subscription) {
    if (!isActiveInstallSource(item, controller)) return;
    throw new Error(unavailableMessage);
  }
  if (!controller.setSource(subscription)) return;
  if (!isActiveInstallSource(item, controller)) return;

  let started = false;
  try {
    started = await startEvents();
  } catch (err: unknown) {
    if (!isActiveInstallSource(item, controller)) return;
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
} as const;

async function connectInstallEvents(
  kind: 'vanilla' | 'loader',
  installId: string,
  item: InstallItem,
  displayName: string,
): Promise<void> {
  if (!isActiveInstallItem(item)) return;
  const copy = STREAM_COPY[kind];

  let source: CloseableSource | null = null;
  let watchdog: InstallStreamWatchdog | null = null;

  const stopWatchdog = (): void => {
    watchdog?.stop();
    watchdog = null;
  };

  const startWatchdog = (): void => {
    stopWatchdog();
    watchdog = createInstallStreamWatchdog(async () => {
      if (!isActiveInstallSource(item, source)) {
        stopWatchdog();
        return;
      }
      await reconcileInstallStreamIssue(installId, item, displayName, {
        source,
        closeSource: true,
        reconnect: true,
      });
    });
  };

  const reconcileIssue = (notice: string): void => {
    if (!isActiveInstallSource(item, source)) return;
    stopWatchdog();
    void reconcileInstallStreamIssue(installId, item, displayName, {
      source,
      closeSource: true,
      reconnect: true,
      notice,
    });
  };

  const onProgress = async (data: InstallProgressEvent): Promise<void> => {
    if (!isActiveInstallSource(item, source)) return;
    watchdog?.markProgress();

    const viewModel = installProgressViewModel(data.view_model);
    if (!viewModel) {
      reconcileIssue(copy.invalid);
      return;
    }

    updateActiveDownloadProgress(viewModel);
    if (viewModel.failed) {
      stopWatchdog();
      completeActiveDownload();
      const failure = await recordBackendInstallFailure(
        item,
        displayName,
        installId,
        unresolvedFailureViewModel(viewModel.label),
      );
      showError(failure.summary);
      await onInstallDone();
      return;
    }
    if (data.done || viewModel.terminal) {
      stopWatchdog();
      await onInstallDone(item);
    }
  };

  if (hasNativeDesktopRuntime()) {
    const controller = createPendingInstallEventSource();
    source = controller;
    startWatchdog();

    try {
      await connectNativeInstallEventSource(
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

export function installLoaderVersion(build: LoaderBuildRecord): void {
  if (!build.component_id || !build.build_id || !build.version_id) return;
  const item: InstallItem = {
    versionId: build.version_id,
    loader: {
      componentId: build.component_id,
      buildId: build.build_id,
      minecraftVersion: build.minecraft_version,
      loaderVersion: build.loader_version,
    },
  };
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

async function onInstallDone(completedItem?: InstallItem): Promise<void> {
  connectedInstallId = null;
  completeActiveDownload();
  if (completedItem) clearDownloadFailureForItem(completedItem);

  try {
    const [versionsRes, instancesRes] = await Promise.all([api('GET', '/versions'), api('GET', '/instances')]);
    if (versionsRes.error) throw new Error(versionsRes.error);
    if (instancesRes.error) throw new Error(instancesRes.error);
    const nextVersions = versionsRes.versions || [];
    versions.value = nextVersions;
    instances.value = instancesRes.instances || [];
    lastInstanceId.value = instancesRes.last_instance_id || null;

    if (catalog.value) {
      const installed = new Set<string>(
        nextVersions
          .filter((version: { launchable: boolean }) => version.launchable)
          .map((version: { id: string }) => version.id),
      );
      catalog.value = {
        ...catalog.value,
        versions: catalog.value.versions.map((version) => ({
          ...version,
          installed: installed.has(version.id),
        })),
      };
    }
  } catch (err: unknown) {
    showError(`Install completed, but failed to refresh launcher state: ${errMessage(err)}`);
  }

  try {
    await refreshInstallQueue({ connectActive: true, retryPendingStart: true });
  } catch (err: unknown) {
    showError(`Install queue refresh failed: ${errMessage(err)}`);
  }
}
