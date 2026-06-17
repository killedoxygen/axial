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
  selectedInstance,
  selectedVersion,
  installState,
  installEventSource,
  catalog,
  versions,
  installFailure,
} from './store';
import {
  startInstall,
  updateInstallProgress,
  completeInstall,
  setInstallEventSource,
  recordInstallFailure,
  clearInstallFailureForItem,
  isActiveInstallItem,
  setInstallQueueState,
  clearInstallFailure,
} from './actions';
import { minecraftVersionLabel } from './version-display';
import { toast } from './toast';
import type { LoaderBuildRecord, LoaderComponentId } from './types-loader';
import type {
  InstallActionViewModel,
  InstallFailureViewModel,
  InstallItem,
  InstallQueueActiveViewModel,
  InstallQueueInstallItemViewModel,
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
  const retryFallback = transportFailureAction('retry', 'Retry install', true);
  const dismissFallback = transportFailureAction('dismiss', 'Dismiss', true);
  const repairFallback = transportFailureAction('repair', 'Automatic repair unavailable', false);
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

function transportFailureAction(action: string, label: string, enabled: boolean): InstallActionViewModel {
  return {
    action,
    label,
    enabled,
    disabled_reason: enabled ? null : 'No automatic repair is available for this failure.',
  };
}

function boundedTransportFailureMessage(message: string): string {
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

function transportFailureViewModel(message: string): InstallFailureViewModel {
  const summary = boundedTransportFailureMessage(message);
  return {
    state_id: 'transport_failure',
    title: 'Install failed',
    tone: 'err',
    summary,
    detail: null,
    details: [],
    retry_action: transportFailureAction('retry', 'Retry install', true),
    dismiss_action: transportFailureAction('dismiss', 'Dismiss', true),
    repair_action: transportFailureAction('repair', 'Automatic repair unavailable', false),
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

function installItemFromQueueInstallItem(value: InstallQueueInstallItemViewModel): InstallItem {
  const versionId = value.version_id;
  if (!value.loader) return { versionId };
  return {
    versionId,
    loader: {
      componentId: value.loader.component_id,
      buildId: value.loader.build_id,
      minecraftVersion: value.loader.minecraft_version,
      loaderVersion: value.loader.loader_version,
    },
  };
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

function applyInstallQueueState(response: InstallQueueStateResponse, options: { showNotice?: boolean } = {}): void {
  setInstallQueueState(response);
  if (options.showNotice) showInstallQueueNotice(response.notice);
}

export async function refreshInstallQueue(
  options: { connectActive?: boolean; retryPendingStart?: boolean } = {},
): Promise<void> {
  for (let attempt = 0; attempt < (options.retryPendingStart ? 4 : 1); attempt += 1) {
    const response = await api<InstallQueueStateResponse>('GET', '/install/queue');
    applyInstallQueueState(response);
    if (options.connectActive) await connectBackendActiveInstall(response.active ?? null);
    if (!options.retryPendingStart || response.active || response.items.length === 0) return;
    await new Promise((resolve) => window.setTimeout(resolve, 120));
  }
}

async function applyInstallQueueResponse(
  response: InstallQueueStateResponse,
  options: { showNotice?: boolean; connectActive?: boolean } = {},
): Promise<void> {
  applyInstallQueueState(response, { showNotice: options.showNotice });
  if (options.connectActive) await connectBackendActiveInstall(response.active ?? null);
}

async function connectBackendActiveInstall(active: InstallQueueActiveViewModel | null): Promise<void> {
  if (!active?.install_id) return;
  if (connectedInstallId === active.install_id && installState.value.status === 'active') return;
  const item = installItemFromQueueInstallItem(active.install_item);
  connectedInstallId = active.install_id;
  startInstall(item, active.progress.label, active.label);
  applyInstallProgressViewModel(active.progress);
  try {
    if (active.kind === 'loader') {
      await connectLoaderEvents(active.install_id, item, active.label);
    } else {
      await connectVanillaEvents(active.install_id, item, active.label);
    }
  } catch (err: unknown) {
    const message = errMessage(err);
    recordInstallFailure(item, active.label, transportFailureViewModel(message));
    showError(`Install progress failed: ${message}`);
    await onInstallDone();
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

export function handleInstallClick(): void {
  const inst = selectedInstance.value;
  if (!inst) return;

  const version = selectedVersion.value;
  const target = version?.needs_install || version?.id || inst.version_id;
  const loader = version?.loader
    ? {
        componentId: version.loader.component_id as LoaderComponentId,
        buildId: version.loader.build_id,
        minecraftVersion: minecraftVersionLabel(version, ''),
        loaderVersion: version.loader.loader_version,
        versionId: target,
      }
    : null;
  if (loader) {
    installLoaderVersion({
      subject_kind: 'loader_build',
      component_id: loader.componentId,
      component_name: '',
      build_id: loader.buildId,
      minecraft_version: loader.minecraftVersion,
      loader_version: loader.loaderVersion,
      version_id: loader.versionId,
      build_meta: {
        terms: [],
        evidence: [],
        selection: {
          default_rank: 0,
          reason: 'unknown',
          source: 'none',
        },
        display_tags: [],
      },
      strategy: '',
      artifact_kind: '',
      installability: '',
    });
    return;
  }

  installVersion(target);
}

export function installVersion(target: string): void {
  if (!target) return;
  const item: InstallItem = { versionId: target };
  clearInstallFailureForItem(item);
  void enqueueBackendInstall(installQueueRequestFromItem(item));
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
  clearInstallFailureForItem(item);
  void enqueueBackendInstall(installQueueRequestFromItem(item));
}

export function retryFailedInstall(): void {
  const failure = installFailure.value;
  const retryAction = failure?.viewModel.retry_action;
  if (!failure || retryAction?.enabled === false) return;
  void enqueueBackendInstall(installQueueRequestFromItem(failure.item), { retry: true }).then(() => {
    clearInstallFailure();
  });
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

async function enqueueBackendInstall(request: InstallQueueRequest, options: { retry?: boolean } = {}): Promise<void> {
  try {
    const response = await api<InstallQueueStateResponse>(
      'POST',
      options.retry ? '/install/queue/retry' : '/install/queue',
      request,
    );
    await applyInstallQueueResponse(response, { showNotice: true, connectActive: true });
  } catch (err: unknown) {
    const message = errMessage(err);
    showError(`Install queue failed: ${message}`);
  }
}

async function connectVanillaEvents(installId: string, item: InstallItem, displayName: string): Promise<void> {
  if (!isActiveInstall(item)) return;

  let progressSource: CloseableSource | null = null;

  const onProgress = async (data: InstallProgressEvent): Promise<void> => {
    if (!isActiveInstallSource(item, progressSource)) return;

    const viewModel = installProgressViewModel(data.view_model);
    if (!viewModel) {
      const message = 'Install progress data was invalid. Retry from Downloads.';
      recordInstallFailure(item, displayName, transportFailureViewModel(message));
      showError(message);
      await onInstallDone();
      return;
    }

    applyInstallProgressViewModel(viewModel);
    if (viewModel.failed) {
      const failure = await recordBackendInstallFailure(
        item,
        displayName,
        installId,
        transportFailureViewModel(viewModel.label),
      );
      showError(failure.summary);
      await onInstallDone();
      return;
    }
    if (data.done || viewModel.terminal) await onInstallDone(item);
  };

  if (hasNativeDesktopRuntime()) {
    const controller = createPendingInstallEventSource();
    progressSource = controller;

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
    return;
  }

  const es = new EventSource(apiUrl(`/install/${installId}/events`));
  progressSource = es;
  setInstallEventSource(es);

  es.addEventListener('progress', (e: MessageEvent) => {
    let data: InstallProgressEvent;
    try {
      data = JSON.parse(e.data) as InstallProgressEvent;
    } catch {
      void (async () => {
        if (isActiveInstallSource(item, es)) {
          const message = 'Install progress data was invalid. Retry from Downloads.';
          recordInstallFailure(item, displayName, transportFailureViewModel(message));
          showError(message);
          await onInstallDone();
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
        const message = 'Install progress stopped unexpectedly. Retry the install from the launcher.';
        recordInstallFailure(item, displayName, transportFailureViewModel(message));
        showError(message);
        await onInstallDone();
      }
    })();
  };
}

async function connectLoaderEvents(installId: string, item: InstallItem, displayName: string): Promise<void> {
  if (!isActiveInstall(item)) return;

  let progressSource: CloseableSource | null = null;
  const onProgress = (data: InstallProgressEvent): void => {
    if (!isActiveInstallSource(item, progressSource)) return;

    const viewModel = installProgressViewModel(data.view_model);
    if (!viewModel) {
      onError('Loader install progress data was invalid. Retry from Downloads.');
      return;
    }

    applyInstallProgressViewModel(viewModel);
    if (viewModel.failed) {
      void onBackendFailure(viewModel);
      return;
    }
    if (data.done || viewModel.terminal) void onInstallDone(item);
  };

  const onError = (message: string): void => {
    if (isActiveInstallSource(item, progressSource)) {
      recordInstallFailure(item, displayName, transportFailureViewModel(message));
      showError(message);
      void onInstallDone();
    }
  };

  const onBackendFailure = async (viewModel: InstallProgressViewModel): Promise<void> => {
    if (!isActiveInstallSource(item, progressSource)) return;
    const failure = await recordBackendInstallFailure(
      item,
      displayName,
      installId,
      transportFailureViewModel(viewModel.label),
    );
    showError(failure.summary);
    await onInstallDone();
  };

  if (hasNativeDesktopRuntime()) {
    const controller = createPendingInstallEventSource();
    progressSource = controller;

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
    return;
  }

  const es = connectLoaderInstallSSE(installId, onProgress, onError);

  progressSource = es;
  setInstallEventSource(es);
}

async function onInstallDone(completedItem?: InstallItem): Promise<void> {
  connectedInstallId = null;
  completeInstall();
  if (completedItem) clearInstallFailureForItem(completedItem);

  try {
    const res = await api('GET', '/versions');
    if (res.error) throw new Error(res.error);
    const nextVersions = res.versions || [];
    versions.value = nextVersions;

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
    showError(`Install completed, but failed to refresh versions: ${errMessage(err)}`);
  }

  try {
    await refreshInstallQueue({ connectActive: true, retryPendingStart: true });
  } catch (err: unknown) {
    showError(`Install queue refresh failed: ${errMessage(err)}`);
  }
}
