import { render } from 'preact';
import './styles';
import { App } from './App';
import { startApplicationBootstrap } from './bootstrap';
import { local } from './state';
import { initErrorReporting } from './error-reporting';
import { applyTheme } from './theme';
import { Sound, bindButtonSounds } from './sound';
import {
  applyDesktopChromeAttributes,
  hasNativeDesktopRuntime,
  nativeDesktopCloseBlockedEventName,
  onNativeEvent,
} from './native';
import { toast } from './toast';
import { restoreRoute } from './ui-state';
import { dtoRecord } from './dto-contract';
import { nativePreferencesHydrated, reloadApplication } from './preferences/persistence';

async function init(): Promise<void> {
  if (hasNativeDesktopRuntime()) Sound.enabled = false;
  initErrorReporting();
  await applyDesktopChromeAttributes();

  // Theme before anything else so the first paint is tinted correctly
  if (!hasNativeDesktopRuntime()) {
    applyTheme(local.theme, local.customHue, {
      silent: true, vibrancy: local.customVibrancy, lightness: local.lightness,
    });
  }

  render(<App />, document.getElementById('app')!);
  restoreRoute();
  registerNativeCloseBlockedToast();

  if (!hasNativeDesktopRuntime()) {
    Sound.enabled = local.sounds;
    void Sound.warmup();
    bindButtonSounds();
  }

  window.addEventListener('keydown', (event) => {
    if (!hasNativeDesktopRuntime()) return;
    if (event.key === 'F5' || ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'r')) {
      event.preventDefault();
      event.stopImmediatePropagation();
      void reloadApplication();
    }
  }, true);

  await startApplicationBootstrap();

  const activateSound = (): void => {
    if (hasNativeDesktopRuntime() && !nativePreferencesHydrated()) return;
    Sound.activate();
  };
  window.addEventListener('pointerdown', activateSound, { once: true, capture: true });
  window.addEventListener('touchstart', activateSound, { once: true, capture: true });
  window.addEventListener('keydown', activateSound, { once: true, capture: true });
}

function registerNativeCloseBlockedToast(): void {
  if (!hasNativeDesktopRuntime()) return;
  void onNativeEvent(nativeDesktopCloseBlockedEventName, (data) => {
    const record = dtoRecord(data, 'Desktop close event');
    const message =
      typeof record.error === 'string' && record.error.trim()
        ? record.error.trim()
        : 'Close is blocked while installs or launches are active.';
    toast(message, 'error');
  }).catch((err: unknown) => {
    console.error('Failed to register native close guard listener', err);
  });
}

void init();
