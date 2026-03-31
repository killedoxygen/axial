import { signal } from '@preact/signals';
import type { LocalPrefs } from './types';

// ── Constants ──

export const STORAGE_KEY: string = 'croopor_ui';
export const PRESET_HUES: Record<string, number> = { obsidian: 140, deepslate: 215, nether: 15, end: 268, birch: 100 };
export const LOGO_BASE_HUE: number = 106;

// ── Local preferences (localStorage-backed) ──

export const defaults: LocalPrefs = {
  theme: 'obsidian',
  customHue: 140,
  customVibrancy: 100,
  lightness: 0,
  logExpanded: false,
  logHeight: 0,
  collapsedGroups: {},
  sidebarFilter: 'all',
  sounds: true,
  shortcuts: {},
  lastUpdateCheckAt: '',
  dismissedUpdateVersion: '',
};

export function loadLocalState(): LocalPrefs {
  try { const r: string | null = localStorage.getItem(STORAGE_KEY); return r ? { ...defaults, ...JSON.parse(r) } : { ...defaults }; } catch { return { ...defaults }; }
}

export const local: LocalPrefs = loadLocalState();
export const localStateVersion = signal(0);

export function saveLocalState(): void {
  try { localStorage.setItem(STORAGE_KEY, JSON.stringify(local)); } catch {}
  localStateVersion.value += 1;
}
