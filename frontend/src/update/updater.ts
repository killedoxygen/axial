import { batch, signal } from '@preact/signals';
import { local, saveLocalState, canEditPreferences } from '../app/local-state';
import { isApiError } from '../platform/transport';
import { applyUpdate, checkForUpdate, downloadUpdate, updateFlow as readUpdateFlow, updateSnapshot } from './api';
import { toast, errMessage } from '../app/toast';
import { hasNativeDesktopRuntime, openExternalURL, requestNativeAppRestart } from '../platform/native';
import { appVersion, bootstrapState } from '../app/store';
import { launchState } from '../launch/state';
import { activeDownload, downloadQueue } from '../install/queue';
import { Sound } from '../app/sound';
import type { UpdateFlow } from '../generated/UpdateFlow';
import type { UpdateInfo } from '../generated/UpdateInfo';
import type { UpdatePhase } from '../generated/UpdatePhase';

const AUTO_CHECK_INTERVAL_MS = 4 * 60 * 60 * 1000;
const AUTO_CHECK_DELAY_MS = 1600;
const AUTO_CHECK_RETRY_MS = 15000;
const AUTO_CHECK_FAILURE_RETRY_DELAYS_MS = [60 * 1000, 5 * 60 * 1000, 15 * 60 * 1000, 60 * 60 * 1000] as const;
const FLOW_POLL_MS = 350;

export const idleUpdateFlow: UpdateFlow = {
  revision: 0,
  phase: 'idle',
  version: '',
  received_bytes: 0,
  total_bytes: null,
  percent: null,
  message: '',
  supported: false,
  can_check: false,
  can_download: false,
  can_apply: false,
  can_restart: false,
};

let autoCheckTimer: number | null = null;
let autoCheckFailureCount = 0;
let autoApplyOnReady = false;
let pendingCheck: Promise<UpdateInfo | null> | null = null;
let pendingCheckSeq = 0;
let pendingCheckToken: symbol | null = null;
let flowPollTimer: number | null = null;
let flowPollPending = false;
let updateRequestSequence = 0;
let pendingUpdateRequest: {
  kind: 'download' | 'apply';
  version: string;
  revision: number;
  response: 'pending' | 'unknown' | 'returned';
} | null = null;

export const updateInfo = signal<UpdateInfo | null>(null);
export const updateCheckState = signal<'idle' | 'checking' | 'ready' | 'error'>('idle');
export const updateFlow = signal<UpdateFlow>(idleUpdateFlow);
export const updateRestartRequested = signal(false);

function displayVersion(version: string): string {
  return version.startsWith('v') ? version : `v${version}`;
}

function updaterSurfaceAvailable(): boolean {
  return hasNativeDesktopRuntime() || __AXIAL_MOCK_API__;
}

function stampUpdateCheck(): void {
  if (!canEditPreferences()) return;
  local.lastUpdateCheckAt = new Date().toISOString();
  saveLocalState();
}

function resetAutoCheckFailureBackoff(): void {
  autoCheckFailureCount = 0;
}

function nextFailedAutoCheckDelay(): number {
  const delay =
    AUTO_CHECK_FAILURE_RETRY_DELAYS_MS[Math.min(autoCheckFailureCount, AUTO_CHECK_FAILURE_RETRY_DELAYS_MS.length - 1)];
  autoCheckFailureCount += 1;
  return delay;
}

export function hasVisibleUpdate(): boolean {
  const info = updateInfo.value;
  return !!(info?.available && local.dismissedUpdateVersion !== info.latest_version);
}

export function updateFlowActive(): boolean {
  return updateFlow.value.phase !== 'idle';
}

export function canInstallUpdateInApp(): boolean {
  return updateInfo.value !== null && updaterSurfaceAvailable();
}

export function dismissAvailableUpdate(): void {
  if (!canEditPreferences()) return;
  const info = updateInfo.value;
  if (!info?.available) return;
  local.dismissedUpdateVersion = info.latest_version;
  saveLocalState();
  toast(`Hidden update ${displayVersion(info.latest_version)} for now`);
}

export async function openUpdateAction(): Promise<void> {
  const url = updateInfo.value?.action_url;
  if (!url) return;
  try {
    await openExternalURL(url);
    toast('Opened latest release');
  } catch (err: unknown) {
    toast(`Failed to open release: ${errMessage(err)}`, 'error');
  }
}

export async function openUpdateNotes(): Promise<void> {
  const url = updateInfo.value?.notes_url;
  if (!url) return;
  try {
    await openExternalURL(url);
    toast('Opened release notes');
  } catch (err: unknown) {
    toast(`Failed to open release notes: ${errMessage(err)}`, 'error');
  }
}

export async function openUpdateChecksum(): Promise<void> {
  const url = updateInfo.value?.checksum_url;
  if (!url) return;
  try {
    await openExternalURL(url);
    toast('Opened release checksum');
  } catch (err: unknown) {
    toast(`Failed to open checksum: ${errMessage(err)}`, 'error');
  }
}

export function restartBlockedByActivity(): boolean {
  return (
    activeDownload.value !== null ||
    downloadQueue.value.view_model.queued_count > 0 ||
    launchState.value.status !== 'idle'
  );
}

export async function restartDesktopApp(): Promise<void> {
  if (!hasNativeDesktopRuntime()) {
    toast('Restart is only available in the desktop app', 'error');
    return;
  }
  if (restartBlockedByActivity()) {
    toast('Restart is blocked while downloads or launches are active.', 'error');
    return;
  }
  try {
    updateRestartRequested.value = true;
    const requested = await requestNativeAppRestart();
    if (!requested) throw new Error('desktop runtime unavailable');
  } catch (err: unknown) {
    updateRestartRequested.value = false;
    toast(`Failed to restart: ${errMessage(err)}`, 'error');
  }
}

function announceUpdateFlowTransition(previous: UpdateFlow, next: UpdateFlow): void {
  if (previous.phase === next.phase) return;
  if (next.phase === 'failed') {
    toast(next.message || 'Update failed', 'error');
  }
}

function setUpdateFlow(next: UpdateFlow, applyOnReady = true): void {
  const previous = updateFlow.value;
  updateFlow.value = next;
  announceUpdateFlowTransition(previous, next);
  if (next.phase === 'failed') autoApplyOnReady = false;
  if (previous.phase !== 'ready' && next.phase === 'ready') {
    Sound.ui('affirm');
    if (autoApplyOnReady && applyOnReady) {
      if (restartBlockedByActivity()) {
        toast(`Update ${displayVersion(next.version)} ready. It installs once downloads and games finish.`);
      }
    } else {
      toast(`Update ${displayVersion(next.version)} downloaded. Restart to install.`);
    }
  }
  if (next.phase === 'ready' && autoApplyOnReady && applyOnReady && !restartBlockedByActivity()) {
    void applyUpdateAndRestart();
  }
}

function updateFlowPollActive(phase: UpdatePhase): boolean {
  return phase === 'downloading' || phase === 'applying';
}

function updateFlowNeedsPolling(): boolean {
  const phase = updateFlow.value.phase;
  return updateFlowPollActive(phase) || (phase === 'ready' && autoApplyOnReady);
}

function scheduleUpdateFlowPoll(): void {
  if (flowPollTimer != null) return;
  flowPollTimer = window.setTimeout(() => {
    flowPollTimer = null;
    void pollUpdateFlow();
  }, FLOW_POLL_MS);
}

async function pollUpdateFlow(): Promise<void> {
  if (flowPollPending || pendingUpdateRequest?.response === 'pending') return;
  flowPollPending = true;
  const sequence = updateRequestSequence;
  try {
    const next = await readUpdateFlow();
    if (sequence !== updateRequestSequence || next.revision < updateFlow.value.revision) return;
    const request = pendingUpdateRequest;
    if (request?.response === 'unknown') {
      if (next.version !== request.version || next.revision <= request.revision) return;
      if (request.kind === 'download' && next.phase === 'idle') return;
      if (request.kind === 'apply' && next.phase !== 'applying' && !next.can_restart) return;
    }
    pendingUpdateRequest = null;
    if (next.phase === 'idle') autoApplyOnReady = false;
    setUpdateFlow(next);
    await restartInstalledUpdate();
  } catch {
    // An unavailable read cannot settle an accepted or possibly accepted command.
  } finally {
    flowPollPending = false;
    if ((pendingUpdateRequest && pendingUpdateRequest.response !== 'pending') || updateFlowNeedsPolling())
      scheduleUpdateFlowPoll();
  }
}

function beginUpdateRequest(kind: 'download' | 'apply', version: string): boolean {
  if (pendingUpdateRequest || updateFlow.value.can_restart) return false;
  pendingUpdateRequest = { kind, version, revision: updateFlow.value.revision, response: 'pending' };
  updateRequestSequence++;
  return true;
}

function recoverUpdateRequest(error: unknown): void {
  if (!pendingUpdateRequest) return;
  const returned =
    isApiError(error) &&
    typeof error.payload === 'object' &&
    error.payload !== null &&
    [
      'update_unsupported',
      'update_busy',
      'update_stale_release',
      'update_not_ready',
      'update_failed',
      'invalid_update_request',
    ].includes(String((error.payload as { code?: unknown }).code));
  pendingUpdateRequest.response = returned ? 'returned' : 'unknown';
  if (returned) autoApplyOnReady = false;
  toast(
    returned ? `Update request failed: ${errMessage(error)}` : 'Update response unavailable. Checking update status.',
    'error',
  );
  void pollUpdateFlow();
}

export async function startUpdateDownload(): Promise<void> {
  const info = updateInfo.value;
  if (!info?.available) return;
  if (!canInstallUpdateInApp()) {
    await openUpdateAction();
    return;
  }
  if (updateFlowPollActive(updateFlow.value.phase)) return;
  if (!beginUpdateRequest('download', info.latest_version)) return;
  try {
    const next = await downloadUpdate(info.latest_version);
    pendingUpdateRequest = null;
    setUpdateFlow(next);
    if (updateFlowNeedsPolling()) scheduleUpdateFlowPoll();
  } catch (err: unknown) {
    recoverUpdateRequest(err);
  }
}

export async function downloadAndInstallUpdate(): Promise<void> {
  const info = updateInfo.value;
  if (!info?.available) return;
  if (!canInstallUpdateInApp()) {
    await openUpdateAction();
    return;
  }
  if (updateFlow.value.phase === 'ready') {
    await applyUpdateAndRestart();
    return;
  }
  autoApplyOnReady = true;
  await startUpdateDownload();
}

export async function applyUpdateAndRestart(): Promise<void> {
  if (updateFlow.value.phase !== 'ready') return;
  if (restartBlockedByActivity()) {
    toast('Finish downloads and close running games before updating.', 'error');
    return;
  }
  if (!beginUpdateRequest('apply', updateFlow.value.version)) return;
  autoApplyOnReady = false;
  try {
    const next = await applyUpdate();
    pendingUpdateRequest = null;
    setUpdateFlow(next);
  } catch (err: unknown) {
    recoverUpdateRequest(err);
    return;
  }
  if (updateFlowPollActive(updateFlow.value.phase)) {
    scheduleUpdateFlowPoll();
    return;
  }
  await restartInstalledUpdate();
}

async function restartInstalledUpdate(): Promise<void> {
  if (updateFlow.value.phase !== 'restart-pending' || updateRestartRequested.value) return;
  if (!hasNativeDesktopRuntime()) {
    toast('Update applied. Restart Axial to finish.');
    return;
  }
  await restartDesktopApp();
}

export async function checkForUpdates(options: { force?: boolean; silent?: boolean } = {}): Promise<UpdateInfo | null> {
  const { force = false, silent = false } = options;
  if (!force && pendingCheck) return pendingCheck;

  const checkSeq = ++pendingCheckSeq;
  const checkToken = Symbol('update-check');
  pendingCheckToken = checkToken;
  updateCheckState.value = 'checking';
  const request = (async () => {
    try {
      const res = await checkForUpdate(force);
      if (checkSeq === pendingCheckSeq) {
        updateInfo.value = res;
        if (
          canEditPreferences() &&
          res.available &&
          local.dismissedUpdateVersion &&
          local.dismissedUpdateVersion !== res.latest_version
        ) {
          local.dismissedUpdateVersion = '';
        }
        updateCheckState.value = 'ready';
        stampUpdateCheck();
        resetAutoCheckFailureBackoff();
        if (!silent) {
          if (res.available) toast(`Update ${displayVersion(res.latest_version)} available`);
          else toast(`You already have ${displayVersion(appVersion.value)}`);
        }
      }
      return res;
    } catch (err: unknown) {
      if (checkSeq === pendingCheckSeq) {
        updateCheckState.value = 'error';
        if (!silent) toast(`Failed to check updates: ${errMessage(err)}`, 'error');
      }
      return null;
    } finally {
      if (pendingCheckToken === checkToken) {
        pendingCheck = null;
        pendingCheckToken = null;
      }
    }
  })();
  pendingCheck = request;

  return request;
}

export function scheduleAutoUpdateCheck(): void {
  if (!updaterSurfaceAvailable()) return;
  queueAutoUpdateCheck(AUTO_CHECK_DELAY_MS);
}

async function runAutoUpdateCheck(): Promise<void> {
  if (!updaterSurfaceAvailable()) return;
  if (
    bootstrapState.value !== 'ready' ||
    pendingCheck ||
    pendingUpdateRequest ||
    updateFlowPollActive(updateFlow.value.phase)
  ) {
    queueAutoUpdateCheck(AUTO_CHECK_RETRY_MS);
    return;
  }
  const checkSequence = pendingCheckSeq;
  const requestSequence = updateRequestSequence;
  const flowRevision = updateFlow.value.revision;
  try {
    const { flow, info } = await updateSnapshot();
    const canCheck = flow.can_check;
    if (
      checkSequence !== pendingCheckSeq ||
      requestSequence !== updateRequestSequence ||
      flowRevision !== updateFlow.value.revision ||
      flow.revision < flowRevision ||
      pendingCheck ||
      pendingUpdateRequest
    ) {
      queueAutoUpdateCheck(AUTO_CHECK_RETRY_MS);
      return;
    }
    batch(() => {
      updateInfo.value = info;
      if (info) updateCheckState.value = 'ready';
      setUpdateFlow(flow, false);
    });
    if (updateFlowPollActive(flow.phase)) scheduleUpdateFlowPoll();
    if (!canCheck) {
      queueAutoUpdateCheck(AUTO_CHECK_INTERVAL_MS);
      return;
    }
  } catch {
    queueAutoUpdateCheck(nextFailedAutoCheckDelay());
    return;
  }
  if (
    activeDownload.value !== null ||
    downloadQueue.value.view_model.queued_count > 0 ||
    launchState.value.status !== 'idle'
  ) {
    queueAutoUpdateCheck(AUTO_CHECK_RETRY_MS);
    return;
  }
  const info = await checkForUpdates({ silent: true });
  if (!info) {
    queueAutoUpdateCheck(nextFailedAutoCheckDelay());
    return;
  }
  queueAutoUpdateCheck(AUTO_CHECK_INTERVAL_MS);
}

function queueAutoUpdateCheck(delay: number): void {
  if (autoCheckTimer != null) window.clearTimeout(autoCheckTimer);
  autoCheckTimer = window.setTimeout(() => {
    autoCheckTimer = null;
    void runAutoUpdateCheck();
  }, delay);
}
