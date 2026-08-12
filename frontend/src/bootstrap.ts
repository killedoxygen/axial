import { api, initializeApiBase } from './api';
import { preloadDeferredViews } from './App';
import { dtoError } from './dto-contract';
import {
  configResponse,
  instancesResponse,
  launcherStatusResponse,
  musicStatusResponse,
  systemInfoResponse,
  versionsResponse,
} from './dto-core';
import { refreshInstallQueue } from './machines/downloads';
import { Music } from './music';
import { getNativeAppVersion } from './native';
import { refreshAccountSkin } from './player-skin';
import { local } from './state';
import {
  appVersion,
  bootstrapError,
  bootstrapState,
  config,
  devMode,
  instances,
  lastInstanceId,
  systemInfo,
  versions,
} from './store';
import { startupWarningMessages } from './startup-warnings';
import { applyTheme } from './theme';
import { toast } from './toast';
import { showOnboardingOverlay } from './ui-state';
import { scheduleAutoUpdateCheck } from './updater';
import { errMessage } from './utils';

let apiInitialized = false;
let activeAttempt: Promise<void> | null = null;

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
  systemInfo.value = systemRes;
  devMode.value = statusRes.dev_mode;
  Music.setTrackCount(musicStatusRes?.count);

  if (statusRes.setup_required) {
    const setupError = dtoError(await api('POST', '/setup/init'));
    if (setupError) throw new Error(setupError);
    statusRes = { ...statusRes, setup_required: false };
  }

  const [versionsRes, instancesRes] = await Promise.all([
    api('GET', '/versions').then(versionsResponse),
    api('GET', '/instances').then(instancesResponse),
  ]);
  versions.value = versionsRes.versions;
  instances.value = instancesRes.instances;
  lastInstanceId.value = instancesRes.last_instance_id;
  await refreshInstallQueue({ connectActive: true }).catch((error: unknown) => {
    console.error('Failed to hydrate the install queue', error);
  });

  if (configRes.theme && local.theme === 'obsidian' && configRes.theme !== 'obsidian') {
    applyTheme(configRes.theme, configRes.custom_hue ?? local.customHue, {
      silent: true,
      vibrancy: configRes.custom_vibrancy ?? local.customVibrancy,
      lightness: configRes.lightness ?? local.lightness,
    });
  }

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
