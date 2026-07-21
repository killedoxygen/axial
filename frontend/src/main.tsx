import { render } from 'preact';
import './styles';
import { App, preloadDeferredViews } from './App';
import { local } from './state';
import {
  appVersion,
  bootstrapError,
  bootstrapState,
  config,
  instances,
  lastInstanceId,
  systemInfo,
  versions,
  devMode,
} from './store';
import { api, initializeApiBase } from './api';
import { initErrorReporting } from './error-reporting';
import { applyTheme } from './theme';
import { Sound, bindButtonSounds } from './sound';
import { Music } from './music';
import {
  applyDesktopChromeAttributes,
  getNativeAppVersion,
  hasNativeDesktopRuntime,
  nativeDesktopCloseBlockedEventName,
  onNativeEvent,
} from './native';
import { refreshAccountSkin } from './player-skin';
import { scheduleAutoUpdateCheck } from './updater';
import { refreshInstallQueue } from './machines/downloads';
import { toast } from './toast';
import { errMessage } from './utils';
import { restoreRoute, showOnboardingOverlay } from './ui-state';
import { startupWarningMessages } from './startup-warnings';

async function init(): Promise<void> {
  initErrorReporting();
  await applyDesktopChromeAttributes();

  // Theme before anything else so the first paint is tinted correctly
  applyTheme(local.theme, local.customHue, {
    silent: true,
    vibrancy: local.customVibrancy,
    lightness: local.lightness,
  });

  render(<App />, document.getElementById('app')!);
  restoreRoute();
  registerNativeCloseBlockedToast();

  Sound.enabled = local.sounds;
  void Sound.warmup();
  bindButtonSounds();

  try {
    await initializeApiBase();

    const nativeVersion = await getNativeAppVersion();
    if (nativeVersion) appVersion.value = nativeVersion;

    let [configRes, systemRes, statusRes, musicStatusRes] = await Promise.all([
      api('GET', '/config'),
      api('GET', '/system').catch(() => null),
      api('GET', '/status').catch(() => null),
      api('GET', '/music/status').catch(() => null),
    ]);
    config.value = configRes;
    systemInfo.value = systemRes;
    devMode.value = statusRes?.dev_mode === true;
    Music.setTrackCount(musicStatusRes?.count);

    let setupRequired = statusRes?.setup_required === true;
    if (setupRequired) {
      try {
        const setupRes = await api('POST', '/setup/init');
        if (setupRes?.error) throw new Error(setupRes.error);
        configRes = {
          ...configRes,
          library_dir: setupRes.library_dir,
          library_mode: setupRes.library_mode,
        };
        config.value = configRes;
        statusRes = {
          ...statusRes,
          setup_required: false,
          library_dir: setupRes.library_dir,
          library_mode: setupRes.library_mode,
        };
        setupRequired = false;
      } catch (err: unknown) {
        toast(`Could not create the managed library: ${errMessage(err)}`, 'error');
      }
    }

    if (!setupRequired) {
      const [versionsRes, instancesRes] = await Promise.all([api('GET', '/versions'), api('GET', '/instances')]);
      versions.value = versionsRes.versions || [];
      instances.value = instancesRes.instances || [];
      lastInstanceId.value = instancesRes.last_instance_id || null;
      await refreshInstallQueue({ connectActive: true });
    } else {
      versions.value = [];
      instances.value = [];
      lastInstanceId.value = null;
    }

    // Apply backend-persisted theme if our local default won
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
    if (!setupRequired) refreshAccountSkin();

    for (const startupWarning of startupWarningMessages(statusRes?.warnings)) {
      toast(startupWarning, 'info');
    }

    if (!setupRequired && configRes && configRes.onboarding_done === false) {
      showOnboardingOverlay.value = true;
    } else if (!setupRequired && Music.enabled) {
      const startMusic = (): void => {
        void Music.play();
      };
      window.addEventListener('pointerdown', startMusic, { once: true, capture: true });
      window.addEventListener('keydown', startMusic, { once: true, capture: true });
    }

    try {
      scheduleAutoUpdateCheck();
    } catch (err: unknown) {
      console.error('Failed to schedule update check', err);
    }
  } catch (err: unknown) {
    bootstrapError.value = errMessage(err);
    bootstrapState.value = 'error';
  }

  const activateSound = (): void => {
    Sound.activate();
  };
  window.addEventListener('pointerdown', activateSound, { once: true, capture: true });
  window.addEventListener('touchstart', activateSound, { once: true, capture: true });
  window.addEventListener('keydown', activateSound, { once: true, capture: true });
}

function scheduleDeferredViewWarmup(): void {
  const warm = (): void => preloadDeferredViews();
  // WebKitGTK lacks requestIdleCallback; a short delay keeps the warmup off
  // the first interactive frames there.
  if (typeof window.requestIdleCallback === 'function') {
    window.requestIdleCallback(warm, { timeout: 3000 });
  } else {
    window.setTimeout(warm, 400);
  }
}

function registerNativeCloseBlockedToast(): void {
  if (!hasNativeDesktopRuntime()) return;
  void onNativeEvent(nativeDesktopCloseBlockedEventName, (data: any) => {
    const message =
      typeof data?.error === 'string' && data.error.trim()
        ? data.error.trim()
        : 'Close is blocked while installs or launches are active.';
    toast(message, 'error');
  }).catch((err: unknown) => {
    console.error('Failed to register native close guard listener', err);
  });
}

void init();
