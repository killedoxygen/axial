import { api, initializeApiBase } from './api';
import { preloadDeferredViews } from './App';
import { dtoError } from './dto-contract';
import {
  configResponse,
  launcherStatusResponse,
  musicStatusResponse,
  systemInfoResponse,
} from './dto-core';
import { refreshInstallQueue } from './machines/downloads';
import { reconnectLaunchSession } from './launch';
import { launchSessionsResponse } from './launch-response-adapters';
import { Music } from './music';
import { getNativeAppVersion, hasNativeDesktopRuntime } from './native';
import { initializeNativePreferences, nativePreferencesHydrated } from './preferences/persistence';
import { local } from './state';
import { Sound, bindButtonSounds } from './sound';
import { refreshAccountSkin } from './player-skin';
import {
  appVersion,
  bootstrapError,
  bootstrapState,
  config,
  devMode,
  instances,
  launchSessions,
  systemInfo,
} from './store';
import { startupWarningMessages } from './startup-warnings';
import { applyConfigTheme, applyTheme } from './theme';
import { toast } from './toast';
import { route, showOnboardingOverlay } from './ui-state';
import { scheduleAutoUpdateCheck } from './updater';
import { errMessage } from './utils';

let apiInitialized = false;
let activeAttempt: Promise<void> | null = null;
let nativeInterfaceReady = false;

export function startApplicationBootstrap(): Promise<void> {
  if (activeAttempt) return activeAttempt;

  bootstrapError.value = null;
  bootstrapState.value = 'loading';
  activeAttempt = runApplicationBootstrap()
    .catch((error: unknown) => {
      bootstrapError.value = errMessage(error);
      bootstrapState.value = 'error';
    })
    .finally(() => {
      activeAttempt = null;
    });
  return activeAttempt;
}

async function runApplicationBootstrap(): Promise<void> {
  if (!apiInitialized) {
    await initializeApiBase();
    apiInitialized = true;
  }

  const nativeVersionRequest = getNativeAppVersion().catch(() => null);
  let [configRes, statusRes, systemRes, musicStatusRes] = await Promise.all([
    api('GET', '/config').then(configResponse),
    api('GET', '/status').then(launcherStatusResponse),
    api('GET', '/system')
      .then(systemInfoResponse)
      .catch(() => null),
    api('GET', '/music/status')
      .then(musicStatusResponse)
      .catch(() => null),
  ]);
  const nativeVersion = await nativeVersionRequest;
  if (nativeVersion) appVersion.value = nativeVersion;

  config.value = configRes;
  if (hasNativeDesktopRuntime() && !nativeInterfaceReady) {
    if (!nativePreferencesHydrated()) {
      const saved = await initializeNativePreferences(configRes);
      Object.assign(local, saved.preferences);
      route.value = saved.route ?? { name: 'home' };
    }
    applyTheme(local.theme, local.customHue, {
      silent: true, vibrancy: local.customVibrancy, lightness: local.lightness,
    });
    Sound.enabled = local.sounds;
    void Sound.warmup();
    bindButtonSounds();
    nativeInterfaceReady = true;
  }
  systemInfo.value = systemRes;
  devMode.value = statusRes.dev_mode;
  Music.setTrackCount(musicStatusRes?.count);

  if (statusRes.setup_required) {
    const setupError = dtoError(await api('POST', '/setup/init'));
    if (setupError) throw new Error(setupError);
    statusRes = { ...statusRes, setup_required: false };
  }

  const [sessionsRes] = await Promise.all([
    api('GET', '/launch/sessions').then(launchSessionsResponse),
    refreshInstallQueue({ connectActive: true, requireInstalledState: true }),
  ]);
  launchSessions.value = sessionsRes;
  for (const instanceId of Object.keys(sessionsRes)) {
    reconnectLaunchSession(instanceId, instances.value.find((instance) => instance.id === instanceId)?.name ?? instanceId);
  }

  applyConfigTheme(configRes);

  Music.applyConfig(configRes);
  bootstrapError.value = null;
  bootstrapState.value = 'ready';
  scheduleDeferredViewWarmup();
  refreshAccountSkin();

  for (const startupWarning of startupWarningMessages(statusRes.warnings)) {
    toast(startupWarning, 'info');
  }

  if (configRes.onboarding_done === false) {
    showOnboardingOverlay.value = true;
  } else if (Music.enabled) {
    const startMusic = (): void => {
      void Music.play();
    };
    window.addEventListener('pointerdown', startMusic, { once: true, capture: true });
    window.addEventListener('keydown', startMusic, { once: true, capture: true });
  }

  try {
    scheduleAutoUpdateCheck();
  } catch (error: unknown) {
    console.error('Failed to schedule update check', error);
  }
}

function scheduleDeferredViewWarmup(): void {
  const warm = (): void => preloadDeferredViews();
  if (typeof window.requestIdleCallback === 'function') {
    window.requestIdleCallback(warm, { timeout: 3000 });
  } else {
    window.setTimeout(warm, 400);
  }
}
