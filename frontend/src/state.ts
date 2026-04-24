import { signal } from '@preact/signals';
import type { LocalPrefs } from './types';

// ── Constants ──

export const STORAGE_KEY: string = 'croopor_ui';
export const PRESET_HUES: Record<string, number> = { obsidian: 140, deepslate: 215, nether: 15, end: 268, birch: 100 };

// ── Local preferences (localStorage-backed) ──

export const defaults: LocalPrefs = {
  theme: 'obsidian',
  customHue: 140,
  customVibrancy: 100,
  lightness: 0,
  logHeight: 0,
  collapsedGroups: {},
  sidebarFilter: 'all',
  sounds: true,
  shortcuts: {},
  lastUpdateCheckAt: '',
  dismissedUpdateVersion: '',
};

export function loadLocalState(): LocalPrefs {
  try {
    const raw: string | null = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...defaults };
    const { logExpanded: _ignored, ...saved } = JSON.parse(raw) as Partial<LocalPrefs & { logExpanded?: boolean }>;
    return { ...defaults, ...saved };
  } catch {
    return { ...defaults };
  }
}

export const local: LocalPrefs = loadLocalState();
export const localStateVersion = signal(0);

export function saveLocalState(): void {
  try { localStorage.setItem(STORAGE_KEY, JSON.stringify(local)); } catch {}
  localStateVersion.value += 1;
}
