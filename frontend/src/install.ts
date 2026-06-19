import { api, apiUrl } from './api';
import { showError, errMessage } from './utils';
import { connectLoaderInstallSSE } from './loaders/api';
import {
  hasNativeDesktopRuntime,
  nativeInstallEventName,
  nativeLoaderInstallEventName,
  onNativeEvent,
  startNativeInstallEvents,
  startNativeLoaderInstallEvents,
} from './native';
import {
  installState,
  installEventSource,
  catalog,
  versions,
  instances,
  lastInstanceId,
  installFailure,
} from './store';
import {
  updateInstallProgress,
  completeInstall,
  setInstallEventSource,
  recordInstallFailure,
  clearInstallFailureForItem,
  isActiveInstallItem,
  isSameInstallItem,
  reconcileInstallQueueState,
  type InstallQueueActiveState,
} from './actions';
import { installItemFromQueueInstallItem, installItemFromQueuedViewModel } from './instance-install-status';
import { toast } from './toast';
import type { LoaderBuildRecord } from './types-loader';
import type {
  InstallActionViewModel,
  InstallFailureViewModel,
  InstallItem,
  InstallQueueActiveViewModel,
  InstallQueueNoticeViewModel,
  InstallQueueRequest,
  InstallQueueStateResponse,
  InstallProgressViewModel,
  InstallStatusResponse,
} from './types-install';
import type { InstallStepProgress } from './store';

type InstallProgressEvent = {
  phase?: string;
  current?: number;
  total?: number;
  file?: string;
  error?: string;
  done?: boolean;
  view_model?: InstallProgressViewModel;
};

type CloseableSource = {
  close(): void;
};

type PendingInstallEventSource = CloseableSource & {
  setSource(source: CloseableSource): boolean;
};

let connectedInstallId: string | null = null;
const INSTALL_STREAM_SILENCE_MS = 15_000;
const INSTALL_RECONCILE_ATTEMPTS = 4;
const INSTALL_RECONCILE_DELAY_MS = 160;
const reconcilingInstallIds = new Set<string>();

function isActiveInstall(item: InstallItem): boolean {
  return isActiveInstallItem(item);
}

function installProgressStepViewModel(value: unknown): InstallStepProgress | undefined {
  if (!value || typeof value !== 'object') return undefined;
  const candidate = value as {
    phase_id?: unknown;
    label?: unknown;
    progress_pct?: unknown;
    current?: unknown;
    total?: unknown;
  };
  if (typeof candidate.phase_id !== 'string' || typeof candidate.label !== 'string') return undefined;
  const pct =
    typeof candidate.progress_pct === 'number' && Number.isFinite(candidate.progress_pct) ? candidate.progress_pct : 0;
  return {
    phase: candidate.phase_id,
    label: candidate.label,
    pct: Math.max(0, Math.min(100, pct)),
    current:
      typeof candidate.current === 'number' && Number.isFinite(candidate.current) ? candidate.current : undefined,
    total: typeof candidate.total === 'number' && Number.isFinite(candidate.total) ? candidate.total : undefined,
  };
}

function installProgressViewModel(value: unknown): InstallProgressViewModel | null {
  if (!value || typeof value !== 'object') return null;
  const candidate = value as Partial<InstallProgressViewModel>;
  if (typeof candidate.phase_id !== 'string' || typeof candidate.label !== 'string') return null;
  const pct =
    typeof candidate.progress_pct === 'number' && Number.isFinite(candidate.progress_pct) ? candidate.progress_pct : 0;
  return {
    phase_id: candidate.phase_id,
    label: candidate.label,
    progress_pct: Math.max(0, Math.min(100, pct)),
    terminal: candidate.terminal === true,
    failed: candidate.failed === true,
    active_step: candidate.active_step ?? null,
  };
}

function installActionViewModel(value: unknown, fallback: InstallActionViewModel): InstallActionViewModel {
  if (!value || typeof value !== 'object') return fallback;
  const candidate = value as Partial<InstallActionViewModel>;
  if (typeof candidate.action !== 'string' || typeof candidate.label !== 'string') return fallback;
  return {
    action: candidate.action,
    label: candidate.label.trim() || fallback.label,
    enabled: candidate.enabled === true,
    disabled_reason:
      typeof candidate.disabled_reason === 'string' && candidate.disabled_reason.trim()
        ? candidate.disabled_reason.trim()
        : null,
  };
}

function installFailureViewModel(value: unknown): InstallFailureViewModel | null {
  if (!value || typeof value !== 'object') return null;
  const candidate = value as Partial<InstallFailureViewModel>;
  if (
    typeof candidate.state_id !== 'string' ||
    typeof candidate.title !== 'string' ||
    typeof candidate.tone !== 'string' ||
    typeof candidate.summary !== 'string'
  ) {
    return null;
  }
  const retryFallback = unavailableFailureAction('retry', 'Retry unavailable');
  const dismissFallback = dismissFailureAction();
  const repairFallback = unavailableFailureAction('repair', 'Repair unavailable');
  return {
    state_id: candidate.state_id,
    title: candidate.title.trim() || 'Install failed',
    tone: candidate.tone.trim() || 'err',
    summary: candidate.summary.trim() || 'Install failed.',
    detail: typeof candidate.detail === 'string' && candidate.detail.trim() ? candidate.detail.trim() : null,
    details: Array.isArray(candidate.details)
      ? candidate.details.filter((detail): detail is string => typeof detail === 'string' && detail.trim().length > 0)
      : [],
    retry_action: installActionViewModel(candidate.retry_action, retryFallback),
    dismiss_action: installActionViewModel(candidate.dismiss_action, dismissFallback),
    repair_action: installActionViewModel(candidate.repair_action, repairFallback),
  };
}

function unavailableFailureAction(action: string, label: string): InstallActionViewModel {
  return {
    action,
    label,
    enabled: false,
    disabled_reason: 'Action unavailable until Croopor receives backend failure details.',
  };
}

function dismissFailureAction(): InstallActionViewModel {
  return {
    action: 'dismiss',
    label: 'Dismiss',
    enabled: true,
    disabled_reason: null,
  };
}

function boundedFailureMessage(message: string): string {
  const firstUsefulLine = String(message || '')
    .split(/\r?\n/)
    .map((line) => line.trim())
    .find((line) => line && !line.startsWith('at '));
  const squashed = (firstUsefulLine || 'Install failed before Croopor received error details.')
    .replace(/\s+/g, ' ')
    .trim();
  if (squashed.length <= 220) return squashed;
  return `${squashed.slice(0, 217).trimEnd()}...`;
}

function unresolvedFailureViewModel(message: string): InstallFailureViewModel {
  const summary = boundedFailureMessage(message);
  return {
    state_id: 'failure_details_unavailable',
    title: 'Install failed',
    tone: 'err',
    summary,
    detail: null,
    details: [],
    retry_action: unavailableFailureAction('retry', 'Retry unavailable'),
    dismiss_action: dismissFailureAction(),
    repair_action: unavailableFailureAction('repair', 'Repair unavailable'),
  };
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
    await new Promise((resolve) => window.setTimeout(resolve, 120));
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
  recordInstallFailure(item, displayName, viewModel);
  return viewModel;
}

function installQueueRequestFromItem(item: InstallItem): InstallQueueRequest {
  if (!item.loader) {
    return {
      kind: 'vanilla',
      version_id: item.versionId,
    };
  }
  return {
    kind: 'loader',
    component_id: item.loader.componentId,
    build_id: item.loader.buildId,
  };
}

function queueNoticeToastKind(notice: InstallQueueNoticeViewModel): 'success' | 'error' | 'info' {
  if (notice.tone === 'error' || notice.tone === 'err') return 'error';
  if (notice.tone === 'warn' || notice.tone === 'warning') return 'info';
  return notice.state_id === 'queued' || notice.state_id === 'retry_queued' ? 'success' : 'info';
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
    await new Promise((resolve) => window.setTimeout(resolve, 120));
  }
  if (latestResponse) return latestResponse;
  throw new Error('Install queue did not return a response.');
}

function installQueueActiveState(
  active: InstallQueueActiveViewModel | null | undefined,
): InstallQueueActiveState | null {
  if (!active) return null;
  return {
    installId: active.install_id ?? undefined,
    operationId: active.operation_id ?? undefined,
    item: installItemFromQueueInstallItem(active.install_item),
    displayName: active.label,
    pct: active.progress.progress_pct,
    label: active.progress.label,
    phase: active.progress.phase_id,
    activeStep: installProgressStepViewModel(active.progress.active_step),
  };
}

export async function applyInstallQueueResponse(
  response: InstallQueueStateResponse,
  options: { showNotice?: boolean; connectActive?: boolean } = {},
): Promise<InstallQueueStateResponse> {
  const terminalActive = response.active?.progress.failed || response.active?.progress.terminal;
  const activeResponse = terminalActive ? { ...response, active: null } : response;
  reconcileInstallQueueState(activeResponse, installQueueActiveState(activeResponse.active));
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
  if (connectedInstallId === active.install_id && installState.value.status === 'active' && installEventSource.value) {
    return;
  }
  const item = installItemFromQueueInstallItem(active.install_item);
  connectedInstallId = active.install_id;
  try {
    if (active.kind === 'loader') {
      await connectLoaderEvents(active.install_id, item, active.label);
    } else {
      await connectVanillaEvents(active.install_id, item, active.label);
    }
  } catch (err: unknown) {
    const message = errMessage(err);
    connectedInstallId = null;
    setInstallEventSource(null);
    showError(`Install progress connection failed: ${message}`);
    await reconcileInstallStreamIssue(active.install_id, item, active.label, {
      reconnect: false,
      notice: 'Install progress connection failed; refreshed backend status.',
    });
  }
}

function applyInstallProgressViewModel(viewModel: InstallProgressViewModel): void {
  updateInstallProgress(
    viewModel.progress_pct,
    viewModel.label,
    viewModel.phase_id,
    undefined,
    installProgressStepViewModel(viewModel.active_step),
  );
}

function isActiveInstallSource(item: InstallItem, source: CloseableSource | null): boolean {
  return isActiveInstall(item) && source !== null && installEventSource.value === source;
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

    applyInstallProgressViewModel(viewModel);

    const failureViewModel = installFailureViewModel(status.failure_view_model);
    if (viewModel.failed || failureViewModel) {
      const failure = failureViewModel ?? unresolvedFailureViewModel(viewModel.label);
      recordInstallFailure(item, displayName, failure);
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
  if (!isActiveInstall(item) || reconcilingInstallIds.has(installId)) return;
  reconcilingInstallIds.add(installId);

  try {
    if (options.closeSource && options.source && installEventSource.value === options.source) {
      connectedInstallId = null;
      setInstallEventSource(null);
    }

    const resolved = await reconcileInstallStatus(installId, item, displayName);
    if (resolved || !isActiveInstall(item)) return;

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
  setInstallEventSource(controller);

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
    if (installEventSource.value === controller) setInstallEventSource(null);
    throw err;
  }

  if (!started) {
    controller.close();
    if (installEventSource.value === controller) setInstallEventSource(null);
    throw new Error(unavailableMessage);
  }
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
  const failure = installFailure.value;
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
  const response = await enqueueBackendInstall(installQueueRequestFromItem(item), options);
  if (installQueueResponseContainsItem(response, item)) clearInstallFailureForItem(item);
  return response;
}

async function enqueueBackendInstall(
  request: InstallQueueRequest,
  options: { retry?: boolean } = {},
): Promise<InstallQueueStateResponse> {
  const response = await api<InstallQueueStateResponse>(
    'POST',
    options.retry ? '/install/queue/retry' : '/install/queue',
    request,
  );
  return applyInstallQueueResponse(response, { showNotice: true, connectActive: true });
}

function showInstallQueueError(err: unknown): void {
  showError(`Install queue failed: ${errMessage(err)}`);
}

async function connectVanillaEvents(installId: string, item: InstallItem, displayName: string): Promise<void> {
  if (!isActiveInstall(item)) return;

  let progressSource: CloseableSource | null = null;
  let watchdog: InstallStreamWatchdog | null = null;

  const stopWatchdog = (): void => {
    watchdog?.stop();
    watchdog = null;
  };

  const startWatchdog = (): void => {
    stopWatchdog();
    watchdog = createInstallStreamWatchdog(async () => {
      if (!isActiveInstallSource(item, progressSource)) {
        stopWatchdog();
        return;
      }
      await reconcileInstallStreamIssue(installId, item, displayName, {
        source: progressSource,
        closeSource: true,
        reconnect: true,
      });
    });
  };

  const onProgress = async (data: InstallProgressEvent): Promise<void> => {
    if (!isActiveInstallSource(item, progressSource)) return;
    watchdog?.markProgress();

    const viewModel = installProgressViewModel(data.view_model);
    if (!viewModel) {
      stopWatchdog();
      await reconcileInstallStreamIssue(installId, item, displayName, {
        source: progressSource,
        closeSource: true,
        reconnect: true,
        notice: 'Install progress data was invalid; refreshed backend status.',
      });
      return;
    }

    applyInstallProgressViewModel(viewModel);
    if (viewModel.failed) {
      stopWatchdog();
      completeInstall();
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
    progressSource = controller;
    startWatchdog();

    try {
      await connectNativeInstallEventSource(
        item,
        controller,
        nativeInstallEventName(installId),
        (data) => {
          void onProgress(data);
        },
        () => startNativeInstallEvents(installId),
        'Desktop install progress is unavailable.',
      );
    } catch (err: unknown) {
      stopWatchdog();
      throw err;
    }
    return;
  }

  const es = new EventSource(apiUrl(`/install/${installId}/events`));
  progressSource = es;
  setInstallEventSource(es);
  startWatchdog();

  es.addEventListener('progress', (e: MessageEvent) => {
    let data: InstallProgressEvent;
    try {
      data = JSON.parse(e.data) as InstallProgressEvent;
    } catch {
      void (async () => {
        if (isActiveInstallSource(item, es)) {
          stopWatchdog();
          await reconcileInstallStreamIssue(installId, item, displayName, {
            source: es,
            closeSource: true,
            reconnect: true,
            notice: 'Install progress data was invalid; refreshed backend status.',
          });
        }
      })();
      return;
    }
    void onProgress(data);
  });

  es.onerror = () => {
    if (es.readyState !== EventSource.CLOSED) return;
    void (async () => {
      if (isActiveInstallSource(item, es)) {
        stopWatchdog();
        await reconcileInstallStreamIssue(installId, item, displayName, {
          source: es,
          closeSource: true,
          reconnect: true,
          notice: 'Install progress stopped unexpectedly; refreshed backend status.',
        });
      }
    })();
  };
}

async function connectLoaderEvents(installId: string, item: InstallItem, displayName: string): Promise<void> {
  if (!isActiveInstall(item)) return;

  let progressSource: CloseableSource | null = null;
  let watchdog: InstallStreamWatchdog | null = null;

  const stopWatchdog = (): void => {
    watchdog?.stop();
    watchdog = null;
  };

  const startWatchdog = (): void => {
    stopWatchdog();
    watchdog = createInstallStreamWatchdog(async () => {
      if (!isActiveInstallSource(item, progressSource)) {
        stopWatchdog();
        return;
      }
      await reconcileInstallStreamIssue(installId, item, displayName, {
        source: progressSource,
        closeSource: true,
        reconnect: true,
      });
    });
  };

  const onProgress = (data: InstallProgressEvent): void => {
    if (!isActiveInstallSource(item, progressSource)) return;
    watchdog?.markProgress();

    const viewModel = installProgressViewModel(data.view_model);
    if (!viewModel) {
      onError('Loader install progress data was invalid.');
      return;
    }

    applyInstallProgressViewModel(viewModel);
    if (viewModel.failed) {
      stopWatchdog();
      completeInstall();
      void onBackendFailure(viewModel);
      return;
    }
    if (data.done || viewModel.terminal) {
      stopWatchdog();
      void onInstallDone(item);
    }
  };

  const onError = (message: string): void => {
    if (isActiveInstallSource(item, progressSource)) {
      stopWatchdog();
      void reconcileInstallStreamIssue(installId, item, displayName, {
        source: progressSource,
        closeSource: true,
        reconnect: true,
        notice: `${message} Refreshed backend status.`,
      });
    }
  };

  const onBackendFailure = async (viewModel: InstallProgressViewModel): Promise<void> => {
    const failure = await recordBackendInstallFailure(
      item,
      displayName,
      installId,
      unresolvedFailureViewModel(viewModel.label),
    );
    showError(failure.summary);
    await onInstallDone();
  };

  if (hasNativeDesktopRuntime()) {
    const controller = createPendingInstallEventSource();
    progressSource = controller;
    startWatchdog();

    try {
      await connectNativeInstallEventSource(
        item,
        controller,
        nativeLoaderInstallEventName(installId),
        (data) => {
          onProgress(data);
        },
        () => startNativeLoaderInstallEvents(installId),
        'Desktop loader install progress is unavailable.',
      );
    } catch (err: unknown) {
      stopWatchdog();
      throw err;
    }
    return;
  }

  const es = connectLoaderInstallSSE(installId, onProgress, onError);

  progressSource = es;
  setInstallEventSource(es);
  startWatchdog();
}

async function onInstallDone(completedItem?: InstallItem): Promise<void> {
  connectedInstallId = null;
  completeInstall();
  if (completedItem) clearInstallFailureForItem(completedItem);

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
