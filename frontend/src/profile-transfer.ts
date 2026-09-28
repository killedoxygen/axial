import {
  LOCAL_PREFERENCES_KEY,
  ROUTE_PREFERENCES_KEY,
  PREFERENCE_BYTES_LIMIT,
  parseLocalPreferences,
  parseRoutePreference,
} from './preferences/local';
import type { LocalPrefs } from './types-ui';
import type { Route } from './ui-state';
import { DEFAULT_SKINS } from './default-skins';

export interface PreferenceStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export interface PreferenceProfile {
  format: 'axial-browser-preferences';
  version: 1;
  preferences: LocalPrefs;
  route: Route | null;
}

function parseJson(serialized: string): unknown {
  if (typeof serialized !== 'string' || new TextEncoder().encode(serialized).length > PREFERENCE_BYTES_LIMIT) {
    throw new Error('The preference profile exceeds the supported size.');
  }
  try {
    return JSON.parse(serialized);
  } catch {
    throw new Error('The preference profile is not valid JSON.');
  }
}

/** Fully validates before a caller offers an import action. */
export function previewPreferenceImport(serialized: string): PreferenceProfile {
  const value = parseJson(serialized);
  if (!value || typeof value !== 'object' || Array.isArray(value))
    throw new Error('Unsupported preference profile format.');
  const data = value as Record<string, unknown>;
  if (
    data.format !== 'axial-browser-preferences' ||
    data.version !== 1 ||
    Object.keys(data).some((key) => !['format', 'version', 'preferences', 'route'].includes(key))
  ) {
    throw new Error('Unsupported preference profile format or version.');
  }
  return {
    format: 'axial-browser-preferences',
    version: 1,
    preferences: parseLocalPreferences(data.preferences),
    route: data.route === null ? null : parseRoutePreference(data.route),
  };
}

export function preferenceReferences(profile: PreferenceProfile): {
  accounts: boolean;
  skins: boolean;
  instances: boolean;
} {
  const preferences = profile.preferences;
  const route = profile.route;
  return {
    accounts: Object.keys(preferences.selectedSkinsByAccount).some((key) => key !== 'account:fallback'),
    skins: [preferences.selectedSkin, ...Object.values(preferences.selectedSkinsByAccount)].some((value) =>
      value.trim().startsWith('saved:'),
    ),
    instances: route?.name === 'instance' || Boolean(route && 'target' in route && route.target),
  };
}

/** Mappings are confirmed owner evidence, never guessed from matching names or IDs. */
export function resolvePreferenceProfile(
  profile: PreferenceProfile,
  bindings: {
    accounts: Record<string, string> | null;
    instances: Record<string, string>;
    skins: readonly string[];
    currentAccounts: readonly string[];
    currentSkins: readonly string[];
  },
): PreferenceProfile {
  const accountKeys = new Map<string, string>();
  for (const [source, destination] of Object.entries(bindings.accounts ?? {})) {
    const key = `account:${source.trim().toLowerCase()}`;
    if (accountKeys.has(key)) throw new Error('Imported account references are ambiguous.');
    accountKeys.set(key, `account:${destination}`);
  }
  const skins = new Set(bindings.skins);
  const currentSkins = new Set(bindings.currentSkins);
  function selection(value: string): string {
    const selected = value.trim();
    if (!selected || DEFAULT_SKINS.some((skin) => selected === `default:${skin.id}`)) return value;
    const key = selected.startsWith('saved:') ? selected.slice(6) : '';
    if (!key || !skins.has(key) || !currentSkins.has(key)) {
      throw new Error(
        'A selected skin has not been imported or is no longer available. Import it before applying preferences.',
      );
    }
    return value;
  }
  const selections = Object.entries(profile.preferences.selectedSkinsByAccount).map(([key, value]) => {
    const destination = key === 'account:fallback' ? key : accountKeys.get(key);
    if (
      !destination ||
      (destination !== 'account:fallback' && !bindings.currentAccounts.includes(destination.slice(8)))
    ) {
      throw new Error(
        'A skin selection refers to an unmapped or removed account. Import its identity before applying preferences.',
      );
    }
    return [destination, selection(value)];
  });
  if (new Set(selections.map(([key]) => key)).size !== selections.length)
    throw new Error('Imported skin selections would collide.');
  const selectedSkinsByAccount = Object.fromEntries(selections);
  function instance(id: string): string {
    const mapped = Object.prototype.hasOwnProperty.call(bindings.instances, id) ? bindings.instances[id] : undefined;
    if (!mapped)
      throw new Error(
        'The saved route refers to an instance without a completed, live import. Import it before applying preferences.',
      );
    return mapped;
  }
  let route = profile.route;
  if (route?.name === 'instance') route = { ...route, id: instance(route.id) };
  else if (route && 'target' in route && route.target) route = { ...route, target: instance(route.target) };
  return {
    ...profile,
    preferences: {
      ...profile.preferences,
      selectedSkin: selection(profile.preferences.selectedSkin),
      selectedSkinsByAccount,
    },
    route,
  };
}

export function preferenceSnapshot(storage: PreferenceStorage, preferences: LocalPrefs, route: Route): string {
  return JSON.stringify([
    preferences,
    route,
    storage.getItem(LOCAL_PREFERENCES_KEY),
    storage.getItem(ROUTE_PREFERENCES_KEY),
  ]);
}

export function preferenceImportNeedsReload(error: unknown): boolean {
  return error instanceof Error && 'rollbackIncomplete' in error && error.rollbackIncomplete === true;
}

/** Call before bootstrap or reload after success, so no live draft overwrites imported values. */
export function importPreferenceProfile(
  serialized: string,
  storage: PreferenceStorage,
  reload?: () => void,
): {
  preferences: LocalPrefs;
  route: Route | null;
  requiresReload: true;
} {
  const profile = previewPreferenceImport(serialized);
  const before = [storage.getItem(LOCAL_PREFERENCES_KEY), storage.getItem(ROUTE_PREFERENCES_KEY)];
  const after = [JSON.stringify(profile.preferences), profile.route === null ? null : JSON.stringify(profile.route)];
  const keys = [LOCAL_PREFERENCES_KEY, ROUTE_PREFERENCES_KEY];
  function write(values: (string | null)[]): void {
    keys.forEach((key, index) => {
      const value = values[index];
      if (value === null) storage.removeItem(key);
      else storage.setItem(key, value);
    });
    if (keys.some((key, index) => storage.getItem(key) !== values[index]))
      throw new Error('Preference storage did not retain the import.');
  }
  try {
    write(after);
    reload?.();
  } catch {
    try {
      write(before);
    } catch {
      throw Object.assign(
        new Error(
          'Preference import failed and previous preferences could not be fully restored. Keep the export and reload before retrying.',
        ),
        { rollbackIncomplete: true },
      );
    }
    throw new Error('Preference import failed. Previous preferences were restored.');
  }
  return { preferences: profile.preferences, route: profile.route, requiresReload: true };
}
