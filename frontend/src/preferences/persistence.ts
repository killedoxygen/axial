import { api, isApiError } from '../api';
import { dtoNumber, dtoRecord } from '../dto-contract';
import {
  completeNativePreferences, hasNativeDesktopRuntime, nativePreferencesEventName,
  nativePreferencesRequest, onNativeEvent, pendingNativePreferences,
  type NativePreferencesRequest,
} from '../native';
import { toast } from '../toast';
import type { InterfacePreferences } from '../generated/InterfacePreferences';
import type { InterfacePreferencesSnapshot } from '../generated/InterfacePreferencesSnapshot';
import type { InterfacePreferencesUpdate } from '../generated/InterfacePreferencesUpdate';
import type { Config } from '../types-settings';
import type { LocalPrefs } from '../types-ui';
import type { Route } from '../ui-state';
import { defaultLocalPreferences, parseLocalPreferences, parseRoutePreference } from './local';

const path = '/config/interface-preferences';
const saveError = 'Interface preferences could not be saved. Your changes remain in this window; closing or reloading is paused.';
let accepted: InterfacePreferencesSnapshot | null = null;
let initialPreferences = defaultLocalPreferences();
let initialization: Promise<InterfacePreferences> | null = null;
let listenerReady = false;
let eventRevision = 0;
let localDraft: LocalPrefs | undefined;
let routeDraft: Route | null | undefined;
let running: Promise<void> | null = null;
const seals = new Set<symbol>();
let nativeRequest: { request: NativePreferencesRequest; release: () => void } | null = null;
let importRelease: (() => void) | null = null;
let reloadPending: Promise<boolean> | null = null;
let lastNativeRequest = '0';
let lastNativePhase: NativePreferencesRequest['phase'] = 'release';
let writeFailed = false;

interface PendingWrite {
  update: InterfacePreferencesUpdate;
  value: InterfacePreferences;
  local: LocalPrefs | undefined;
  route: Route | null | undefined;
}
let uncertain: PendingWrite | null = null;

function revision(value: unknown): number {
  const number = dtoNumber(value, 'Interface preference revision');
  if (!Number.isSafeInteger(number) || number < 0) throw new Error('Invalid interface preference revision.');
  return number;
}

function snapshot(value: unknown): InterfacePreferencesSnapshot {
  const record = dtoRecord(value, 'Interface preferences');
  if (record.value === null) return { revision: revision(record.revision), value: null };
  const envelope = dtoRecord(record.value, 'Interface preference value');
  if (envelope.version !== 1) throw new Error('Unsupported interface preference version.');
  return { revision: revision(record.revision), value: {
    version: 1, preferences: parseLocalPreferences(envelope.preferences),
    route: envelope.route === null ? null : parseRoutePreference(envelope.route),
  } };
}

function seal(): () => void {
  const token = Symbol();
  seals.add(token);
  return () => { seals.delete(token); };
}

export function canEditPreferences(): boolean {
  return seals.size === 0 && (!hasNativeDesktopRuntime() || accepted !== null);
}

export function nativePreferencesHydrated(): boolean { return accepted !== null; }

function acknowledge(write: PendingWrite, nextRevision: number): void {
  writeFailed = false;
  accepted = { revision: nextRevision, value: write.value };
  if (localDraft === write.local) localDraft = undefined;
  if (routeDraft === write.route) routeDraft = undefined;
  if (uncertain === write) uncertain = null;
}

async function reconcile(write: PendingWrite): Promise<void> {
  const current = snapshot(await api('GET', path));
  if (current.revision !== write.update.expected_revision + 1 || JSON.stringify(current.value) !== JSON.stringify(write.value)) {
    throw new Error(saveError);
  }
  acknowledge(write, current.revision);
}

async function commit(write: PendingWrite): Promise<void> {
  uncertain = write;
  try {
    const receipt = dtoRecord(await api('PUT', path, write.update), 'Interface preference receipt');
    const nextRevision = revision(receipt.revision);
    if (nextRevision !== write.update.expected_revision + 1) throw new Error(saveError);
    acknowledge(write, nextRevision);
  } catch (error) {
    if (isApiError(error) && error.status >= 400 && error.status < 500) {
      // A definite refusal can be rebased by a later explicit edit or flush.
      uncertain = null;
      accepted = snapshot(await api('GET', path));
      throw new Error(saveError);
    }
    // An older read does not prove the accepted write was cancelled. Retain it
    // and only read again on the next flush; never replay an unknown mutation.
    await reconcile(write);
  }
}

function draftWrite(): PendingWrite {
  if (!accepted) throw new Error('Interface preferences have not loaded.');
  const value: InterfacePreferences = {
    version: 1,
    preferences: localDraft ?? accepted.value?.preferences ?? initialPreferences,
    route: routeDraft === undefined ? accepted.value?.route ?? null : routeDraft,
  };
  const change: InterfacePreferencesUpdate['change'] = !accepted.value || (localDraft && routeDraft !== undefined)
    ? { kind: 'replace', value }
    : localDraft ? { kind: 'local', preferences: localDraft } : { kind: 'route', route: value.route };
  return { update: { expected_revision: accepted.revision, change }, value, local: localDraft, route: routeDraft };
}

export function flushNativePreferences(): Promise<void> {
  if (!hasNativeDesktopRuntime()) return Promise.resolve();
  if (running) return running;
  const work = (async () => {
    if (initialization) await initialization;
    if (!accepted) throw new Error('Interface preferences have not loaded.');
    if (uncertain) await reconcile(uncertain);
    while ((localDraft !== undefined || routeDraft !== undefined) && nativeRequest?.request.phase !== 'discard') {
      await commit(draftWrite());
    }
  })();
  running = work.catch((error: unknown) => { writeFailed = true; throw error; }).finally(() => {
    running = null;
    if (!writeFailed && !uncertain && nativeRequest?.request.phase !== 'discard' &&
        (localDraft !== undefined || routeDraft !== undefined)) scheduleSave();
  });
  return running;
}

function scheduleSave(): void {
  if (running) return;
  void flushNativePreferences().catch(() => toast(saveError, 'error'));
}

export function saveNativeLocalPreferences(preferences: LocalPrefs): void {
  if (!canEditPreferences()) return;
  localDraft = parseLocalPreferences(preferences);
  scheduleSave();
}

export function saveNativeRoute(next: Route): void {
  if (!canEditPreferences()) return;
  routeDraft = parseRoutePreference(next);
  scheduleSave();
}

function handleNativeRequest(request: NativePreferencesRequest): void {
  const number = /^preferences-([1-9][0-9]*)$/.exec(request.request_id)?.[1];
  if (!number) throw new Error('Invalid desktop preference request identity.');
  const newer = number.length > lastNativeRequest.length ||
    (number.length === lastNativeRequest.length && number > lastNativeRequest);
  if (!newer && number !== lastNativeRequest) return;
  if (!newer && lastNativePhase === 'release') return;
  if (newer) {
    lastNativeRequest = number;
    lastNativePhase = request.phase;
  }
  if (request.phase === 'release') {
    lastNativePhase = 'release';
    nativeRequest?.release();
    nativeRequest = null;
    if (accepted && (localDraft !== undefined || routeDraft !== undefined) && !uncertain && !writeFailed) scheduleSave();
    return;
  }
  if (nativeRequest?.request.request_id === request.request_id) return;
  const active = { request, release: seal() };
  nativeRequest?.release();
  nativeRequest = active;
  void (async () => {
    let saved = true;
    try {
      if (request.phase === 'flush') await flushNativePreferences();
      else await running?.catch(() => undefined);
    } catch { saved = false; toast(saveError, 'error'); }
    if (nativeRequest !== active) return;
    try { await completeNativePreferences(request.request_id, saved); }
    catch { toast('Could not confirm interface preference preparation. Keep this window open and try again.', 'error'); }
  })();
}

export function initializeNativePreferences(cfg: Config): Promise<InterfacePreferences> {
  if (initialization) return initialization;
  initialization = (async () => {
    if (!listenerReady) {
      const listener = await onNativeEvent(nativePreferencesEventName, (value) => {
        eventRevision++;
        try { handleNativeRequest(nativePreferencesRequest(value)); }
        catch { toast('Could not read the desktop preference request.', 'error'); }
      });
      if (!listener) throw new Error('Desktop preference coordination is unavailable.');
      listenerReady = true;
    }
    const before = eventRevision;
    const pending = await pendingNativePreferences();
    if (pending && before === eventRevision) handleNativeRequest(pending);
    const loaded = snapshot(await api('GET', path));
    initialPreferences = defaultLocalPreferences();
    if (loaded.value === null && cfg.theme) {
      initialPreferences = parseLocalPreferences({ ...initialPreferences, theme: cfg.theme,
        customHue: cfg.custom_hue ?? initialPreferences.customHue,
        customVibrancy: cfg.custom_vibrancy ?? initialPreferences.customVibrancy,
        lightness: cfg.lightness ?? initialPreferences.lightness });
    }
    accepted = loaded;
    return loaded.value ?? { version: 1, preferences: initialPreferences, route: null };
  })().catch((error: unknown) => { initialization = null; throw error; });
  return initialization;
}

export function nativePreferenceBaseline(preferences: LocalPrefs, currentRoute: Route): string {
  return JSON.stringify([preferences, currentRoute]);
}

function reloadSealed(): void { location.reload(); }

/** Browser callers remain synchronous so preference import keeps its rollback semantics. */
export function reloadApplication(): boolean | Promise<boolean> {
  if (!hasNativeDesktopRuntime()) { reloadSealed(); return true; }
  if (reloadPending) return reloadPending;
  if (nativeRequest) { toast('Reload is paused while the desktop operation finishes.', 'error'); return false; }
  const release = seal();
  reloadPending = flushNativePreferences().then(() => { reloadSealed(); return true; }).catch(() => {
    release();
    toast(saveError, 'error');
    return false;
  }).finally(() => { reloadPending = null; });
  return reloadPending;
}

export async function replaceNativePreferences(value: InterfacePreferences, current: () => boolean): Promise<void> {
  if (!canEditPreferences()) throw new Error('Interface preference changes are paused. Try again after the current operation.');
  const release = seal();
  try {
    await flushNativePreferences();
    if (!current() || nativeRequest) throw new Error('References or current preferences changed. Review the export again.');
    if (!accepted) throw new Error('Interface preferences have not loaded.');
    const normalized: InterfacePreferences = {
      version: 1, preferences: parseLocalPreferences(value.preferences),
      route: value.route === null ? null : parseRoutePreference(value.route),
    };
    const write: PendingWrite = {
      update: { expected_revision: accepted.revision, change: { kind: 'replace', value: normalized } },
      value: normalized, local: undefined, route: undefined,
    };
    await commit(write);
    importRelease = release;
    reloadSealed();
  } catch (error) {
    if (uncertain || importRelease === release) {
      importRelease = release;
      throw Object.assign(new Error('Preference import needs confirmation before reload. Keep this window open and retry Reload.'), { rollbackIncomplete: true });
    }
    release();
    throw error;
  }
}
