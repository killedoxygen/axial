import { render } from 'preact';
import './styles';
import { App } from './App';
import { local } from './state';
import {
  appVersion, bootstrapError, bootstrapState, config, instances, lastInstanceId,
  systemInfo, versions, devMode,
} from './store';
import { api, initializeApiBase } from './api';
import { applyTheme } from './theme';
import { Sound, bindButtonSounds } from './sound';
import { Music } from './music';
import { getNativeAppVersion } from './native';
import { refreshAccountSkin } from './player-skin';
import { scheduleAutoUpdateCheck } from './updater';
import { toast } from './toast';
import { errMessage } from './utils';
import { restoreRoute, showOnboardingOverlay, showSetupOverlay } from './ui-state';

async function init(): Promise<void> {
  // Theme before anything else so the first paint is tinted correctly
  applyTheme(local.theme, local.customHue, {
    silent: true,
    vibrancy: local.customVibrancy,
    lightness: local.lightness,
  });

  render(<App />, document.getElementById('app')!);
  restoreRoute();

  Sound.enabled = local.sounds;
  Sound.warmup();
  bindButtonSounds();

  try {
    await initializeApiBase();

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
    const setupRequired = statusRes?.setup_required === true;
    if (setupRequired) {
      showSetupOverlay.value = true;
      versions.value = [];
      instances.value = [];
      lastInstanceId.value = null;
    } else {
      const [versionsRes, instancesRes] = await Promise.all([
        api('GET', '/versions'),
        api('GET', '/instances'),
      ]);
      versions.value = versionsRes.versions || [];
      instances.value = instancesRes.instances || [];
      lastInstanceId.value = instancesRes.last_instance_id || null;
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
    if (!setupRequired) refreshAccountSkin();

    const startupWarnings = Array.isArray(statusRes?.warnings) ? statusRes.warnings : [];
    const shownStartupWarnings = new Set<string>();
    for (const startupWarning of startupWarnings) {
      if (
        typeof startupWarning === 'string'
        && startupWarning.trim()
        && !shownStartupWarnings.has(startupWarning)
      ) {
        shownStartupWarnings.add(startupWarning);
        toast(startupWarning, 'info');
      }
    }

    if (!setupRequired && configRes && configRes.onboarding_done === false) {
      showOnboardingOverlay.value = true;
    } else if (!setupRequired && Music.enabled) {
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
