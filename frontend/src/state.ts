import { signal } from '@preact/signals';
import type { LocalPrefs } from './types-ui';
import { LOCAL_PREFERENCES_KEY, defaultLocalPreferences, parseLocalPreferences } from './preferences/local';
import { hasNativeDesktopRuntime } from './native';
import { canEditPreferences, saveNativeLocalPreferences } from './preferences/persistence';
export { canEditPreferences } from './preferences/persistence';

export const STORAGE_KEY: string = LOCAL_PREFERENCES_KEY;
export const PRESET_HUES: Record<string, number> = { obsidian: 140, deepslate: 215, nether: 15, end: 268, birch: 100 };

export const defaults: LocalPrefs = defaultLocalPreferences();

export function loadLocalState(): LocalPrefs {
  if (hasNativeDesktopRuntime()) return defaultLocalPreferences();
  try {
    const raw: string | null = localStorage.getItem(STORAGE_KEY);
    if (!raw) return defaultLocalPreferences();
    return parseLocalPreferences(JSON.parse(raw) as unknown);
  } catch {
    return defaultLocalPreferences();
  }
}

export const local: LocalPrefs = loadLocalState();
export const localStateVersion = signal(0);
let persistenceSuspended = false;

/** Keep late callbacks from overwriting imported preferences while this document reloads. */
export function suspendLocalStatePersistence(): () => void {
  const previous = persistenceSuspended;
  persistenceSuspended = true;
  return () => {
    persistenceSuspended = previous;
  };
}

export function saveLocalState(): void {
  if (persistenceSuspended || !canEditPreferences()) return;
  if (hasNativeDesktopRuntime()) {
    saveNativeLocalPreferences(local);
    localStateVersion.value += 1;
    return;
  }
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(local));
  } catch {}
  localStateVersion.value += 1;
}
