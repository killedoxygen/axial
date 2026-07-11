import { signal } from '@preact/signals';
import { local, saveLocalState } from './state';
import { api } from './api';
import { toast } from './toast';
import { hasNativeDesktopRuntime, openExternalURL, requestNativeAppRestart } from './native';
import { appVersion, bootstrapState, launchState, updateCheckState, updateInfo } from './store';
import { activeDownload, downloadQueue } from './machines/downloads';
import { Sound } from './sound';
import { idleUpdateFlow, type UpdateFlowPhase, type UpdateFlowState, type UpdateInfo } from './types-update';
import { errMessage } from './utils';

const AUTO_CHECK_INTERVAL_MS = 4 * 60 * 60 * 1000;
const AUTO_CHECK_DELAY_MS = 1600;
const AUTO_CHECK_RETRY_MS = 15000;
const AUTO_CHECK_FAILURE_RETRY_DELAYS_MS = [60 * 1000, 5 * 60 * 1000, 15 * 60 * 1000, 60 * 60 * 1000] as const;
const FLOW_POLL_MS = 350;

let autoCheckTimer: number | null = null;
let autoCheckFailureCount = 0;
let pendingCheck: Promise<UpdateInfo | null> | null = null;
let pendingCheckSeq = 0;
let pendingCheckToken: symbol | null = null;
let flowPollTimer: number | null = null;

export const updateFlow = signal<UpdateFlowState>(idleUpdateFlow);

function displayVersion(version: string): string {
  return version.startsWith('v') ? version : `v${version}`;
}

function updaterSurfaceAvailable(): boolean {
  return hasNativeDesktopRuntime() || __AXIAL_MOCK_API__;
}

function stampUpdateCheck(): void {
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
  return updateInfo.value?.install_mode === 'in-app' && updaterSurfaceAvailable();
}

export function dismissAvailableUpdate(): void {
  const info = updateInfo.value;
  if (!info?.available) return;
  local.dismissedUpdateVersion = info.latest_version;
  saveLocalState();
  toast(`Hidden update ${displayVersion(info.latest_version)} for now`);
}

export function formatUpdateCheckTime(raw: string): string {
  const stamp = Date.parse(raw || '');
  if (Number.isNaN(stamp)) return 'Not checked yet';
  return new Date(stamp).toLocaleString();
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
    const requested = await requestNativeAppRestart();
    if (!requested) throw new Error('desktop runtime unavailable');
    toast('Restarting Axial');
  } catch (err: unknown) {
    toast(`Failed to restart: ${errMessage(err)}`, 'error');
  }
}

function updateFlowFromResponse(res: unknown): UpdateFlowState {
  const record = (res ?? {}) as Partial<Record<keyof UpdateFlowState, unknown>>;
  const phases: UpdateFlowPhase[] = ['idle', 'downloading', 'ready', 'applying', 'restart-pending', 'failed'];
  const phase = phases.includes(record.phase as UpdateFlowPhase) ? (record.phase as UpdateFlowPhase) : 'idle';
  return {
    phase,
    version: typeof record.version === 'string' ? record.version : '',
    received_bytes: typeof record.received_bytes === 'number' ? record.received_bytes : 0,
    total_bytes: typeof record.total_bytes === 'number' ? record.total_bytes : null,
    percent: typeof record.percent === 'number' ? record.percent : null,
    message: typeof record.message === 'string' ? record.message : '',
  };
}

function announceUpdateFlowTransition(previous: UpdateFlowState, next: UpdateFlowState): void {
  if (previous.phase === next.phase) return;
  if (next.phase === 'ready') {
    Sound.ui('affirm');
    toast(`Update ${displayVersion(next.version)} downloaded. Restart to install.`);
  } else if (next.phase === 'failed') {
    toast(next.message || 'Update failed', 'error');
  }
}

function setUpdateFlow(next: UpdateFlowState): void {
  const previous = updateFlow.value;
  updateFlow.value = next;
  announceUpdateFlowTransition(previous, next);
}

function updateFlowPollActive(phase: UpdateFlowPhase): boolean {
  return phase === 'downloading' || phase === 'applying';
}

function scheduleUpdateFlowPoll(): void {
  if (flowPollTimer != null) return;
  flowPollTimer = window.setTimeout(() => {
    flowPollTimer = null;
    void pollUpdateFlow();
  }, FLOW_POLL_MS);
}

async function pollUpdateFlow(): Promise<void> {
  try {
    const res = await api('GET', '/update/flow');
    setUpdateFlow(updateFlowFromResponse(res));
  } catch {}
  if (updateFlowPollActive(updateFlow.value.phase)) scheduleUpdateFlowPoll();
}

export async function startUpdateDownload(): Promise<void> {
  const info = updateInfo.value;
  if (!info?.available) return;
  if (!canInstallUpdateInApp()) {
    await openUpdateAction();
    return;
  }
  if (updateFlowPollActive(updateFlow.value.phase)) return;
  try {
    const res = await api('POST', '/update/download', { version: info.latest_version });
    setUpdateFlow(updateFlowFromResponse(res));
    if (updateFlowPollActive(updateFlow.value.phase)) scheduleUpdateFlowPoll();
  } catch (err: unknown) {
    toast(`Update download failed: ${errMessage(err)}`, 'error');
  }
}

export async function applyUpdateAndRestart(): Promise<void> {
  if (updateFlow.value.phase !== 'ready') return;
  if (restartBlockedByActivity()) {
    toast('Finish downloads and close running games before updating.', 'error');
    return;
  }
  try {
    const res = await api('POST', '/update/apply');
    setUpdateFlow(updateFlowFromResponse(res));
  } catch (err: unknown) {
    toast(`Failed to apply update: ${errMessage(err)}`, 'error');
    void pollUpdateFlow();
    return;
  }
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
      const res = await api('GET', force ? '/update?force=1' : '/update');
      if (res.error) throw new Error(res.error);
      if (checkSeq === pendingCheckSeq) {
        updateInfo.value = res;
        if (res.available && local.dismissedUpdateVersion && local.dismissedUpdateVersion !== res.latest_version) {
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
    activeDownload.value !== null ||
    downloadQueue.value.view_model.queued_count > 0 ||
    launchState.value.status !== 'idle' ||
    updateFlowPollActive(updateFlow.value.phase)
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
