import { render } from 'preact';
import { App } from './App';
import { local } from './state';
import {
  appVersion, bootstrapError, bootstrapState, config, instances, lastInstanceId,
  systemInfo, versions, devMode,
} from './store';
import { api } from './api';
import { applyTheme } from './theme';
import { Sound, bindButtonSounds } from './sound';
import { Music } from './music';
import { getNativeAppVersion } from './native';
import { scheduleAutoUpdateCheck } from './updater';
import { errMessage } from './utils';
import { restoreRoute, showOnboardingOverlay, showSetupOverlay } from './ui-state';

async function init(): Promise<void> {
  render(<App />, document.getElementById('app')!);

  // Theme before anything else so the first paint is tinted correctly
  applyTheme(local.theme, local.customHue, {
    silent: true,
    vibrancy: local.customVibrancy,
    lightness: local.lightness,
  });
  restoreRoute();

  Sound.enabled = local.sounds;
  Sound.warmup();
  bindButtonSounds();

  try {
    const nativeVersion = await getNativeAppVersion();
    if (nativeVersion) appVersion.value = nativeVersion;

    const [configRes, systemRes, statusRes, musicStatusRes] = await Promise.all([
      api('GET', '/config'),
      api('GET', '/system').catch(() => null),
      api('GET', '/status').catch(() => null),
      api('GET', '/music/status').catch(() => null),
    ]);
    config.value = configRes;
    systemInfo.value = systemRes;
    devMode.value = statusRes?.dev_mode === true;
    Music.setTrackCount(musicStatusRes?.count);

    // Library setup overlay opens when the backend says a library is missing
    if (statusRes?.setup_required) {
      showSetupOverlay.value = true;
    }

    const [versionsRes, instancesRes] = await Promise.all([
      api('GET', '/versions'),
      api('GET', '/instances'),
    ]);
    versions.value = versionsRes.versions || [];
    instances.value = instancesRes.instances || [];
    lastInstanceId.value = instancesRes.last_instance_id || null;

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

    if (configRes && configRes.onboarding_done === false) {
      showOnboardingOverlay.value = true;
    } else if (Music.enabled) {
      const startMusic = (): void => { void Music.play(); };
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

  const activateSound = (): void => { Sound.activate(); };
  window.addEventListener('pointerdown', activateSound, { once: true, capture: true });
  window.addEventListener('touchstart', activateSound, { once: true, capture: true });
  window.addEventListener('keydown', activateSound, { once: true, capture: true });
}

void init();
