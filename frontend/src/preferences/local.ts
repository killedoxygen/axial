import type { LocalPrefs, OverlayPosition, ShortcutBinding } from '../types-ui';
import type { Route } from '../ui-state';

export const LOCAL_PREFERENCES_KEY = 'axial_rewrite_ui';
export const ROUTE_PREFERENCES_KEY = 'axial-rewrite:route';
export const PREFERENCE_BYTES_LIMIT = 1024 * 1024;

export function defaultLocalPreferences(): LocalPrefs {
  return {
    theme: 'obsidian', customHue: 140, customVibrancy: 100, lightness: 0,
    sounds: true, hideSkinNametag: false, selectedSkin: '', selectedSkinsByAccount: {},
    shortcuts: {}, overlayPositions: {}, lastUpdateCheckAt: '', dismissedUpdateVersion: '',
  };
}

function invalid(): never {
  throw new Error('The preference profile contains unsupported or invalid values.');
}

function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) invalid();
  const entries = Object.entries(value);
  if (entries.length > 4096 || entries.some(([key]) => key.length > 1024 || ['__proto__', 'prototype', 'constructor'].includes(key))) invalid();
  return value as Record<string, unknown>;
}

function knownFields(value: Record<string, unknown>, keys: readonly string[]): void {
  if (Object.keys(value).some((key) => !keys.includes(key))) invalid();
}

function text(value: unknown, limit = 1024): string {
  if (typeof value !== 'string' || value.length > limit || /[\u0000-\u001f]/.test(value)) invalid();
  return value;
}

function number(value: unknown, min = -1_000_000, max = 1_000_000): number {
  if (typeof value !== 'number' || !Number.isFinite(value) || value < min || value > max) invalid();
  return value;
}

function bool(value: unknown): boolean {
  if (typeof value !== 'boolean') invalid();
  return value;
}

function shortcut(value: unknown): ShortcutBinding {
  const data = record(value);
  knownFields(data, ['key', 'ctrl', 'shift', 'alt', 'meta']);
  const result: ShortcutBinding = { key: text(data.key, 64) };
  if (!result.key) invalid();
  for (const key of ['ctrl', 'shift', 'alt', 'meta'] as const) {
    if (data[key] !== undefined) result[key] = bool(data[key]);
  }
  return result;
}

function position(value: unknown): OverlayPosition {
  const data = record(value);
  knownFields(data, ['x', 'y', 'scaleX', 'scaleY']);
  const result: OverlayPosition = { x: number(data.x), y: number(data.y) };
  for (const key of ['scaleX', 'scaleY'] as const) {
    if (data[key] !== undefined) result[key] = number(data[key], 0, 1);
  }
  return result;
}

function mapped<T>(value: unknown, parse: (entry: unknown) => T): Record<string, T> {
  return Object.fromEntries(Object.entries(record(value)).sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0)
    .map(([key, entry]) => [key, parse(entry)]));
}

/** Missing fields are predecessor defaults; malformed/unknown fields are never silently discarded. */
export function parseLocalPreferences(value: unknown): LocalPrefs {
  const data = record(value);
  const result = defaultLocalPreferences();
  knownFields(data, Object.keys(result));
  if (data.theme !== undefined) {
    const theme = text(data.theme);
    if (!['obsidian', 'deepslate', 'nether', 'end', 'birch', 'custom'].includes(theme)) invalid();
    result.theme = theme;
  }
  for (const key of ['customHue', 'customVibrancy', 'lightness'] as const) {
    if (data[key] !== undefined) result[key] = number(data[key], 0, key === 'customHue' ? 360 : 100);
  }
  for (const key of ['sounds', 'hideSkinNametag'] as const) {
    if (data[key] !== undefined) result[key] = bool(data[key]);
  }
  for (const key of ['selectedSkin', 'lastUpdateCheckAt', 'dismissedUpdateVersion'] as const) {
    if (data[key] !== undefined) result[key] = text(data[key]);
  }
  if (data.selectedSkinsByAccount !== undefined) result.selectedSkinsByAccount = mapped(data.selectedSkinsByAccount, (entry) => text(entry));
  if (data.shortcuts !== undefined) result.shortcuts = mapped(data.shortcuts, shortcut);
  if (data.overlayPositions !== undefined) result.overlayPositions = mapped(data.overlayPositions, position);
  return result;
}

export function parseRoutePreference(value: unknown): Route {
  const data = record(value);
  const name = text(data.name);
  switch (name) {
    case 'home': case 'instances': case 'dev-lab': case 'downloads': case 'accounts': case 'settings':
      knownFields(data, ['name']);
      return { name };
    case 'instance':
      knownFields(data, ['name', 'id']);
      if (!text(data.id)) invalid();
      return { name, id: text(data.id) };
    case 'discover':
      knownFields(data, ['name', 'target']);
      return data.target === undefined ? { name } : { name, target: text(data.target) };
    case 'content':
      knownFields(data, ['name', 'id', 'target']);
      if (!text(data.id)) invalid();
      return data.target === undefined ? { name, id: text(data.id) } : { name, id: text(data.id), target: text(data.target) };
    default:
      return invalid();
  }
}
