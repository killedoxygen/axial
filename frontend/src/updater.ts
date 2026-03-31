import { local, saveLocalState } from './state';
import { api } from './api';
import { toast } from './toast';
import { openExternalURL, isWailsRuntime } from './native';
import { appVersion, bootstrapState, installState, launchState, updateCheckState, updateInfo } from './store';
import type { UpdateInfo } from './types';
import { errMessage } from './utils';

const AUTO_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;
const AUTO_CHECK_DELAY_MS = 1600;

let autoCheckTimer: number | null = null;
let pendingCheck: Promise<UpdateInfo | null> | null = null;

function displayVersion(version: string): string {
  return version.startsWith('v') ? version : `v${version}`;
}

function shouldAutoCheck(): boolean {
  if (!isWailsRuntime()) return false;
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
}

export function formatUpdateCheckTime(raw: string): string {
  const stamp = Date.parse(raw || '');
  if (Number.isNaN(stamp)) return 'Not checked yet';
  return new Date(stamp).toLocaleString();
}

export async function openUpdateAction(): Promise<void> {
  const url = updateInfo.value?.action_url;
  if (!url) return;
  await openExternalURL(url);
}

export async function openUpdateNotes(): Promise<void> {
  const url = updateInfo.value?.notes_url;
  if (!url) return;
  await openExternalURL(url);
}

export async function checkForUpdates(options: { force?: boolean; silent?: boolean } = {}): Promise<UpdateInfo | null> {
  const { force = false, silent = false } = options;
  if (!force && pendingCheck) return pendingCheck;

  updateCheckState.value = 'checking';
  pendingCheck = (async () => {
    try {
      const res = await api('GET', '/update');
      if (res.error) throw new Error(res.error);
      updateInfo.value = res;
      if (res.available && local.dismissedUpdateVersion && local.dismissedUpdateVersion !== res.latest_version) {
        local.dismissedUpdateVersion = '';
      }
      if (!silent) {
        if (res.available) toast(`Update ${displayVersion(res.latest_version)} available`);
        else toast(`You already have ${displayVersion(appVersion.value)}`);
      }
      updateCheckState.value = 'ready';
      return res;
    } catch (err: unknown) {
      updateCheckState.value = 'error';
      if (!silent) toast(`Failed to check updates: ${errMessage(err)}`, 'error');
      return null;
    } finally {
      stampUpdateCheck();
      pendingCheck = null;
    }
  })();

  return pendingCheck;
}

export function scheduleAutoUpdateCheck(): void {
  if (!shouldAutoCheck()) return;
  if (autoCheckTimer != null) window.clearTimeout(autoCheckTimer);
  autoCheckTimer = window.setTimeout(() => {
    autoCheckTimer = null;
    if (bootstrapState.value !== 'ready') return;
    if (installState.value.status !== 'idle') return;
    if (launchState.value.status !== 'idle') return;
    void checkForUpdates({ silent: true });
  }, AUTO_CHECK_DELAY_MS);
}
