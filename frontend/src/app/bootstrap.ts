import { initializeApiBase } from '../platform/transport';
import { preloadDeferredViews } from './App';
import { applicationStatus, getConfig, initializeSetup, systemResources } from '../settings/api';
import { musicStatus } from '../music/api';
import { listSessions } from '../launch/api';
import { refreshAccountsData } from '../accounts/state';
import { refreshInstallQueue } from '../install/queue';
import { reconnectLaunchSession } from '../launch/controller';
import { Music } from '../music/music';
import { getNativeAppVersion, hasNativeDesktopRuntime } from '../platform/native';
import { initializeNativePreferences, nativePreferencesHydrated } from './preferences/persistence';
import { local } from './local-state';
import { Sound, bindButtonSounds } from './sound';
import { appVersion, bootstrapError, bootstrapFailure, bootstrapState, devMode } from './store';
import { isApiError, isRepeatableRefusal } from '../platform/transport';
import { config, systemInfo } from '../settings/state';
import { instances } from '../instances/state';
import { launchSessions, liveSessions } from '../launch/state';
import { applyConfigTheme, applyTheme } from './theme';
import { toast, errMessage } from './toast';
import { route, showOnboardingOverlay } from './route';
import { scheduleAutoUpdateCheck } from '../update/updater';
import { warmHome } from '../home/HomeView';

let apiInitialized = false;
let activeAttempt: Promise<void> | null = null;
let nativeInterfaceReady = false;

/** Startup attempts made automatically while an owner reports its state busy with other work. */
const BUSY_RETRIES = 8;

/**
 * No response is a connection failure. An owner's typed busy refusal was made before any effect, so startup waits and
 * repeats; every other refusal keeps its own message. A repeat starts from a fresh status read, so setup runs again
 * only while the status owner still requires it and a lost setup response is never replayed blindly.
 */
function bootstrapFailureKind(error: unknown): 'transport' | 'busy' | 'refused' {
  if (!isApiError(error)) return 'transport';
  return isRepeatableRefusal(error) ? 'busy' : 'refused';
}

export function startApplicationBootstrap(): Promise<void> {
  if (activeAttempt) return activeAttempt;

  bootstrapError.value = null;
  bootstrapFailure.value = null;
  bootstrapState.value = 'loading';
  activeAttempt = (async () => {
    for (let attempt = 0; ; attempt++) {
      try {
        await runApplicationBootstrap();
        bootstrapFailure.value = null;
        return;
      } catch (error: unknown) {
        const kind = bootstrapFailureKind(error);
        if (kind === 'busy' && attempt < BUSY_RETRIES) {
          bootstrapFailure.value = 'busy';
          await new Promise((resolve) => setTimeout(resolve, Math.min(8000, 500 * 2 ** attempt)));
          continue;
        }
        bootstrapError.value = errMessage(error);
        bootstrapFailure.value = kind;
        bootstrapState.value = 'error';
        return;
      }
    }
  })().finally(() => {
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
  const [configRes, statusRes, systemRes, musicStatusRes] = await Promise.all([
    getConfig(),
    applicationStatus(),
    systemResources().catch(() => null),
    musicStatus().catch(() => null),
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
      silent: true,
      vibrancy: local.customVibrancy,
      lightness: local.lightness,
    });
    Sound.enabled = local.sounds;
    void Sound.warmup();
    bindButtonSounds();
    nativeInterfaceReady = true;
  }
  systemInfo.value = systemRes;
  devMode.value = statusRes.dev_mode;
  Music.setTrackCount(musicStatusRes?.count);

  if (statusRes.setup_required) await initializeSetup();

  const [sessionsRes] = await Promise.all([
    listSessions().then((response) => liveSessions(response.sessions)),
    refreshInstallQueue({ connectActive: true, requireInstalledState: true }),
    // Who is playing is known before the first frame, wherever the launcher opens; it never rejects.
    refreshAccountsData(),
  ]);
  launchSessions.value = sessionsRes;
  for (const instanceId of Object.keys(sessionsRes)) {
    reconnectLaunchSession(
      instanceId,
      instances.value.find((instance) => instance.id === instanceId)?.name ?? instanceId,
    );
  }

  applyConfigTheme(configRes);

  // Home's picture belongs to its first frame.
  if (route.value.name === 'home') await warmHome();

  Music.applyConfig(configRes);
  bootstrapError.value = null;
  bootstrapState.value = 'ready';
  scheduleDeferredViewWarmup();

  for (const warning of statusRes.warnings) toast(warning, 'info');

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
