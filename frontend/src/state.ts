import { signal } from '@preact/signals';
import type { LocalPrefs } from './types';

export const STORAGE_KEY: string = 'croopor_ui';
export const PRESET_HUES: Record<string, number> = { obsidian: 140, deepslate: 215, nether: 15, end: 268, birch: 100 };

export const defaults: LocalPrefs = {
  theme: 'obsidian',
  customHue: 140,
  customVibrancy: 100,
  lightness: 0,
  logHeight: 0,
  collapsedGroups: {},
  sidebarFilter: 'all',
  sounds: true,
  hideSkinNametag: false,
  selectedSkin: '',
  selectedSkinsByAccount: {},
  shortcuts: {},
  overlayPositions: {},
  lastUpdateCheckAt: '',
  dismissedUpdateVersion: '',
};

export function loadLocalState(): LocalPrefs {
  try {
    const raw: string | null = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...defaults };
    const saved = JSON.parse(raw) as Partial<LocalPrefs>;
    return {
      ...defaults,
      ...saved,
      selectedSkinsByAccount: stringRecord(saved.selectedSkinsByAccount),
    };
  } catch {
    return { ...defaults };
  }
}

export const local: LocalPrefs = loadLocalState();
export const localStateVersion = signal(0);

export function saveLocalState(): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(local));
  } catch {}
  localStateVersion.value += 1;
}

function stringRecord(value: unknown): Record<string, string> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return {};
  const output: Record<string, string> = {};
  for (const [key, entry] of Object.entries(value)) {
    if (typeof entry === 'string') output[key] = entry;
  }
  return output;
}
