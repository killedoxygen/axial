import { local, saveLocalState } from './state';
import { api } from './api';
import { toast } from './toast';
import { hasNativeDesktopRuntime, openExternalURL } from './native';
import { appVersion, bootstrapState, installState, launchState, updateCheckState, updateInfo } from './store';
import type { UpdateInfo } from './types';
import { errMessage } from './utils';

const AUTO_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;
const AUTO_CHECK_DELAY_MS = 1600;
const AUTO_CHECK_RETRY_MS = 15000;

let autoCheckTimer: number | null = null;
let pendingCheck: Promise<UpdateInfo | null> | null = null;
let pendingCheckSeq = 0;
let pendingCheckToken: symbol | null = null;

function displayVersion(version: string): string {
  return version.startsWith('v') ? version : `v${version}`;
}

function shouldAutoCheck(): boolean {
  if (!hasNativeDesktopRuntime()) return false;
  const last = Date.parse(local.lastUpdateCheckAt || '');
  return Number.isNaN(last) || (Date.now() - last) >= AUTO_CHECK_INTERVAL_MS;
}

function stampUpdateCheck(): void {
  local.lastUpdateCheckAt = new Date().toISOString();
  saveLocalState();
}

export function hasVisibleUpdate(): boolean {
  const info = updateInfo.value;
  return !!(info?.available && local.dismissedUpdateVersion !== info.latest_version);
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
  if (!hasNativeDesktopRuntime()) return;
  if (!shouldAutoCheck()) {
    queueAutoUpdateCheck(AUTO_CHECK_INTERVAL_MS);
    return;
  }
  if (autoCheckTimer != null) window.clearTimeout(autoCheckTimer);
  autoCheckTimer = window.setTimeout(() => {
    autoCheckTimer = null;
    void runAutoUpdateCheck();
  }, AUTO_CHECK_DELAY_MS);
}

async function runAutoUpdateCheck(): Promise<void> {
  if (!hasNativeDesktopRuntime()) return;
  if (!shouldAutoCheck()) {
    queueAutoUpdateCheck(AUTO_CHECK_INTERVAL_MS);
    return;
  }
  if (bootstrapState.value !== 'ready' || installState.value.status !== 'idle' || launchState.value.status !== 'idle') {
    queueAutoUpdateCheck(AUTO_CHECK_RETRY_MS);
    return;
  }
  await checkForUpdates({ silent: true });
  if (shouldAutoCheck()) queueAutoUpdateCheck(AUTO_CHECK_RETRY_MS);
  else queueAutoUpdateCheck(AUTO_CHECK_INTERVAL_MS);
}

function queueAutoUpdateCheck(delay: number): void {
  if (autoCheckTimer != null) window.clearTimeout(autoCheckTimer);
  autoCheckTimer = window.setTimeout(() => {
    autoCheckTimer = null;
    void runAutoUpdateCheck();
  }, delay);
}
