import { signal } from '@preact/signals';
import { api, apiUrl } from '../api';
import { DEFAULT_SKINS, type DefaultSkin } from '../default-skins';
import {
  hasSelectedSkinForAccount,
  refreshAccountSkin,
  resetSelectedSkin,
  selectedSkinForAccount,
  setSelectedSkin,
} from '../player-skin';
import { toast } from '../toast';
import { showConfirm } from '../ui/Dialog';
import {
  activeMinecraftSkin,
  apiResponseError,
  boundedMessage,
  commandSummary,
  DEFAULT_SKIN_SOURCE,
  defaultSkinFile,
  defaultSkinTextureKey,
  defaultSkinTextureKeys,
  downloadBlob,
  fetchSavedSkinPng,
  savedSkinApplyErrorMessage,
  savedSkinDownloadFilename,
  savedSkinRecord,
  savedSkinsResponse,
  skinActionErrorMessage,
  skinFlushResult,
  skinVariantValue,
} from '../views/accounts/api';
import type { MinecraftProfile, SavedSkinRecord, SkinVariant } from '../views/accounts/types';
import { refreshAccountsData } from './accounts';

export type WardrobeSelection =
  | { kind: 'saved'; key: string }
  | { kind: 'default'; id: string }
  | { kind: 'profile' }
  | { kind: 'lookup' }
  | { kind: 'none' };

export interface WardrobeContext {
  accountKey: string;
  skinActionsEnabled: boolean;
  profile: MinecraftProfile | null;
}

export interface WardrobeData {
  state: 'loading' | 'ready' | 'unavailable';
  skins: SavedSkinRecord[];
  pendingApplyKey: string | null;
  error: string | null;
}

export type WardrobeOpKind =
  | 'apply'
  | 'flush'
  | 'cancel-pending'
  | 'delete'
  | 'download'
  | 'upload'
  | 'save-profile'
  | 'reset-profile-skin'
  | 'reset-profile-cape'
  | 'cape'
  | 'lookup'
  | 'edit';

export interface WardrobeOp {
  kind: WardrobeOpKind;
  key?: string;
}

const DEFERRED_APPLY_RECHECK_MS = 11_500;

export const wardrobeContext = signal<WardrobeContext>({
  accountKey: 'account:fallback',
  skinActionsEnabled: false,
  profile: null,
});
export const wardrobeData = signal<WardrobeData>({ state: 'loading', skins: [], pendingApplyKey: null, error: null });
export const wardrobeSelection = signal<WardrobeSelection>({ kind: 'none' });
export const wardrobeOp = signal<WardrobeOp | null>(null);
export const wardrobeNotice = signal<string | null>(null);
export const defaultSkinKeys = signal<ReadonlyMap<string, string>>(new Map());
export const defaultSkinKeysReady = signal(false);

let wardrobeRequestId = 0;
let deferredApplyTimer: number | null = null;
let profileSeedRequestId = 0;
let profileSeedKey: string | null = null;
const profileSavedKey = signal<string | null>(null);
let defaultKeysRequested = false;

export function setWardrobeNotice(text: string | null): void {
  wardrobeNotice.value = text;
}

export function wardrobeBusy(): boolean {
  return wardrobeOp.value !== null;
}

function profileSkinKey(profile: MinecraftProfile | null): string | null {
  const skin = activeMinecraftSkin(profile ?? undefined);
  if (!profile || !skin) return null;
  return JSON.stringify([profile.id, skin.id, skin.url, skinVariantValue(skin.variant)]);
}

export function setWardrobeContext(next: WardrobeContext): void {
  const current = wardrobeContext.value;
  const accountChanged = current.accountKey !== next.accountKey;
  const changed =
    accountChanged ||
    current.skinActionsEnabled !== next.skinActionsEnabled ||
    profileSkinKey(current.profile) !== profileSkinKey(next.profile);
  wardrobeContext.value = next;
  if (accountChanged) wardrobeSelection.value = { kind: 'none' };
  if (changed) reconcileWardrobeSelection();
  seedProfileSkin();
}

export async function refreshWardrobe(): Promise<void> {
  const requestId = ++wardrobeRequestId;
  try {
    const parsed = savedSkinsResponse(await api('GET', '/skins'));
    if (requestId !== wardrobeRequestId) return;
    if (!parsed) throw new Error('invalid saved skins response');
    wardrobeData.value = { state: 'ready', skins: parsed.skins, pendingApplyKey: parsed.pendingApplyKey, error: null };
    schedulePendingApplyRecheck(parsed.pendingApplyKey);
  } catch (err: unknown) {
    if (requestId !== wardrobeRequestId) return;
    wardrobeData.value = {
      state: 'unavailable',
      skins: [],
      pendingApplyKey: null,
      error: skinActionErrorMessage(err, 'Saved skins are unavailable.'),
    };
  }
  reconcileWardrobeSelection();
  seedProfileSkin();
}

export function loadDefaultSkinKeys(): void {
  if (defaultKeysRequested) return;
  defaultKeysRequested = true;
  void defaultSkinTextureKeys()
    .then((keys) => {
      defaultSkinKeys.value = keys;
      defaultSkinKeysReady.value = true;
    })
    .catch(() => {
      defaultKeysRequested = false;
    });
}

function rememberDefaultSkinKey(id: string, textureKey: string): void {
  if (defaultSkinKeys.value.get(id) === textureKey) return;
  const next = new Map(defaultSkinKeys.value);
  next.set(id, textureKey);
  defaultSkinKeys.value = next;
}

function schedulePendingApplyRecheck(pendingApplyKey: string | null): void {
  if (deferredApplyTimer !== null) {
    window.clearTimeout(deferredApplyTimer);
    deferredApplyTimer = null;
  }
  if (!pendingApplyKey) return;
  deferredApplyTimer = window.setTimeout(() => {
    deferredApplyTimer = null;
    void refreshWardrobe();
    void refreshAccountsData();
    refreshAccountSkin();
  }, DEFERRED_APPLY_RECHECK_MS);
}

function currentProfileSeedCandidate(): { key: string; accountKey: string; variant: SkinVariant } | null {
  const { accountKey, skinActionsEnabled, profile } = wardrobeContext.value;
  const profileSkin = activeMinecraftSkin(profile ?? undefined);
  if (wardrobeData.value.state !== 'ready' || !skinActionsEnabled || !profile || !profileSkin) return null;
  const variant = skinVariantValue(profileSkin.variant);
  return {
    key: JSON.stringify([accountKey, profile.id, profileSkin.id, profileSkin.url, variant]),
    accountKey,
    variant,
  };
}

function invalidateProfileSeed(): void {
  if (profileSeedKey !== null) profileSeedRequestId += 1;
  profileSeedKey = null;
  profileSavedKey.value = null;
}

export function reconcileWardrobeSelection(): void {
  const { accountKey, skinActionsEnabled, profile } = wardrobeContext.value;
  const data = wardrobeData.value;
  const current = wardrobeSelection.value;
  const profileSkin = activeMinecraftSkin(profile ?? undefined);

  if (current.kind === 'lookup') return;
  if (current.kind === 'profile' && profileSkin) return;

  const preference = selectedSkinForAccount(accountKey);
  if (skinActionsEnabled && profileSkin && !hasSelectedSkinForAccount(accountKey)) {
    wardrobeSelection.value = { kind: 'profile' };
    return;
  }
  if (preference.startsWith('default:')) {
    const id = preference.slice('default:'.length);
    if (DEFAULT_SKINS.some((skin) => skin.id === id)) {
      wardrobeSelection.value = { kind: 'default', id };
      return;
    }
  }
  if (data.state !== 'ready') return;
  if (data.skins.length === 0) {
    if (!preference.startsWith('default:')) resetSelectedSkin(accountKey);
    wardrobeSelection.value = profileSkin ? { kind: 'profile' } : { kind: 'default', id: 'steve' };
    return;
  }
  if (current.kind === 'saved' && data.skins.some((skin) => skin.texture_key === current.key)) return;
  const preferredKey = preference.startsWith('saved:') ? preference.slice('saved:'.length) : null;
  const next =
    (preferredKey ? data.skins.find((skin) => skin.texture_key === preferredKey) : undefined) ??
    data.skins.find((skin) => Boolean(skin.applied_at)) ??
    data.skins[0];
  wardrobeSelection.value = { kind: 'saved', key: next.texture_key };
  if (preferredKey !== next.texture_key) setSelectedSkin(`saved:${next.texture_key}`, accountKey);
}

export function selectSavedSkin(textureKey: string): void {
  wardrobeNotice.value = null;
  wardrobeSelection.value = { kind: 'saved', key: textureKey };
  setSelectedSkin(`saved:${textureKey}`, wardrobeContext.value.accountKey);
}

export function selectDefaultSkin(id: string): void {
  wardrobeNotice.value = null;
  wardrobeSelection.value = { kind: 'default', id };
  setSelectedSkin(`default:${id}`, wardrobeContext.value.accountKey);
}

export function previewProfileSkin(): void {
  wardrobeNotice.value = null;
  wardrobeSelection.value = { kind: 'profile' };
}

export function previewLookupSkin(): void {
  wardrobeNotice.value = null;
  wardrobeSelection.value = { kind: 'lookup' };
}

export function endLookupPreview(): void {
  if (wardrobeSelection.value.kind !== 'lookup') return;
  wardrobeSelection.value = { kind: 'none' };
  reconcileWardrobeSelection();
}

export function returnFromProfilePreview(): void {
  if (wardrobeSelection.value.kind !== 'profile') return;
  wardrobeSelection.value = { kind: 'none' };
  reconcileWardrobeSelection();
}

export function resetWardrobePreview(): void {
  wardrobeNotice.value = null;
  resetSelectedSkin(wardrobeContext.value.accountKey);
  wardrobeSelection.value = { kind: 'default', id: 'steve' };
}

export async function runWardrobeOp<T>(op: WardrobeOp, task: () => Promise<T>): Promise<T | undefined> {
  if (wardrobeOp.value) return undefined;
  wardrobeOp.value = op;
  try {
    return await task();
  } finally {
    wardrobeOp.value = null;
  }
}

async function wardrobeAction(
  op: WardrobeOp,
  fallbackError: string,
  task: () => Promise<string | null>,
): Promise<boolean> {
  if (wardrobeOp.value) return false;
  wardrobeOp.value = op;
  wardrobeNotice.value = null;
  try {
    const summary = await task();
    if (summary) toast(summary);
    return true;
  } catch (err: unknown) {
    wardrobeNotice.value = skinActionErrorMessage(err, fallbackError);
    return false;
  } finally {
    wardrobeOp.value = null;
  }
}

export async function applySavedSkin(textureKey: string, options: { select?: boolean } = {}): Promise<string> {
  const response = await api('POST', `/skins/${textureKey}/apply?defer=true`);
  wardrobeData.value = { ...wardrobeData.value, pendingApplyKey: textureKey };
  if (options.select !== false) selectSavedSkin(textureKey);
  void refreshWardrobe();
  return commandSummary(response, 'Skin command accepted.');
}

export async function applySkin(textureKey: string): Promise<void> {
  const skin = wardrobeData.value.skins.find((saved) => saved.texture_key === textureKey);
  if (skin?.applied_at) return;
  await wardrobeAction({ kind: 'apply', key: textureKey }, 'Could not apply skin.', () => applySavedSkin(textureKey));
}

export async function flushPendingApply(): Promise<void> {
  await wardrobeAction({ kind: 'flush' }, 'Could not apply queued skin.', async () => {
    const result = skinFlushResult(await api('POST', '/skins/flush'));
    if (!result) throw new Error('Skin flush returned an invalid response.');
    wardrobeData.value = { ...wardrobeData.value, pendingApplyKey: null };
    void refreshWardrobe();
    void refreshAccountsData();
    refreshAccountSkin();
    return result.viewModel?.summary ?? 'Skin command accepted.';
  });
}

export async function cancelPendingApply(): Promise<void> {
  await wardrobeAction({ kind: 'cancel-pending' }, 'Could not cancel queued skin apply.', async () => {
    const response = await api('DELETE', '/skins/pending');
    wardrobeData.value = { ...wardrobeData.value, pendingApplyKey: null };
    void refreshWardrobe();
    return commandSummary(response, 'Skin change canceled.');
  });
}

export async function deleteSavedSkin(skin: SavedSkinRecord): Promise<void> {
  const name = skin.name.trim();
  const ok = await showConfirm(
    name
      ? `Delete saved skin "${name}"? This removes it from local saved skins only.`
      : 'Delete this saved skin? This removes it from local saved skins only.',
    { title: 'Delete saved skin', destructive: true, confirmText: 'Delete' },
  );
  if (!ok) return;
  await wardrobeAction({ kind: 'delete', key: skin.texture_key }, 'Could not delete skin.', async () => {
    await api('DELETE', `/skins/${skin.texture_key}`);
    const accountKey = wardrobeContext.value.accountKey;
    if (selectedSkinForAccount(accountKey) === `saved:${skin.texture_key}`) resetSelectedSkin(accountKey);
    if (wardrobeSelection.value.kind === 'saved' && wardrobeSelection.value.key === skin.texture_key) {
      wardrobeSelection.value = { kind: 'none' };
    }
    void refreshWardrobe();
    return name ? `Deleted "${name}"` : 'Skin deleted';
  });
}

export async function downloadSavedSkin(skin: SavedSkinRecord): Promise<void> {
  await wardrobeAction({ kind: 'download', key: skin.texture_key }, 'Could not download skin PNG.', async () => {
    downloadBlob(await fetchSavedSkinPng(skin), savedSkinDownloadFilename(skin));
    return 'Skin PNG downloaded';
  });
}

export async function uploadSkinPng(
  file: File,
  options: {
    name: string;
    variant: SkinVariant;
    capeId?: string | null;
    source?: string;
    applyAfterSave: boolean;
    select?: boolean;
  },
): Promise<SavedSkinRecord | null> {
  const params = new URLSearchParams({ name: options.name, variant: options.variant });
  if (options.capeId) params.set('cape_id', options.capeId);
  if (options.source) params.set('source', options.source);
  const response = await fetch(apiUrl(`/skins?${params.toString()}`), {
    method: 'POST',
    headers: { 'Content-Type': 'image/png' },
    body: file,
  });
  const payload = await response.json().catch(() => undefined);
  if (!response.ok) throw apiResponseError(response, payload, `Upload failed with HTTP ${response.status}`);
  const saved = savedSkinRecord(payload);
  if (saved && options.select !== false) selectSavedSkin(saved.texture_key);
  if (saved && options.applyAfterSave) {
    try {
      toast(await applySavedSkin(saved.texture_key, { select: options.select !== false }));
    } catch (err: unknown) {
      void refreshWardrobe();
      wardrobeNotice.value = savedSkinApplyErrorMessage(err);
    }
  } else {
    void refreshWardrobe();
    if (options.source !== DEFAULT_SKIN_SOURCE) toast('Skin added to your library');
  }
  return saved;
}

export async function saveProfileSkinLocally(): Promise<void> {
  const { skinActionsEnabled, profile } = wardrobeContext.value;
  if (!skinActionsEnabled) return;
  const profileSkin = activeMinecraftSkin(profile ?? undefined);
  await wardrobeAction({ kind: 'save-profile' }, 'Could not save Minecraft profile skin.', async () => {
    const request: { variant?: SkinVariant; mark_current: true } = { mark_current: true };
    if (profileSkin) request.variant = skinVariantValue(profileSkin.variant);
    const saved = savedSkinRecord(await api('POST', '/skins/from-profile', request));
    if (saved) {
      profileSavedKey.value = saved.texture_key;
      selectSavedSkin(saved.texture_key);
    }
    void refreshWardrobe();
    return 'Profile skin added to your library';
  });
}

export async function resetProfileSkin(): Promise<void> {
  const { skinActionsEnabled, profile } = wardrobeContext.value;
  if (!skinActionsEnabled || !activeMinecraftSkin(profile ?? undefined)) return;
  const ok = await showConfirm(
    'Reset the active Minecraft profile skin to the default skin? Croopor will save the current profile skin locally first.',
    { title: 'Reset profile skin', destructive: true, confirmText: 'Reset' },
  );
  if (!ok) return;
  await wardrobeAction({ kind: 'reset-profile-skin' }, 'Could not reset Minecraft profile skin.', async () => {
    const response = await api('POST', '/skin/profile/reset', {});
    void refreshWardrobe();
    void refreshAccountsData();
    refreshAccountSkin();
    return commandSummary(response, 'Skin command accepted.');
  });
}

export async function resetProfileCape(): Promise<void> {
  if (!wardrobeContext.value.skinActionsEnabled) return;
  const ok = await showConfirm(
    'Remove the active Minecraft profile cape? Croopor will save the current skin and cape pairing locally first.',
    { title: 'Reset profile cape', destructive: true, confirmText: 'Reset cape' },
  );
  if (!ok) return;
  await wardrobeAction({ kind: 'reset-profile-cape' }, 'Could not reset Minecraft profile cape.', async () => {
    const response = await api('POST', '/skin/cape/reset', {});
    void refreshWardrobe();
    void refreshAccountsData();
    refreshAccountSkin();
    return commandSummary(response, 'Skin command accepted.');
  });
}

export async function changeSavedSkinCape(skin: SavedSkinRecord, capeId: string | null): Promise<void> {
  if ((skin.cape_id ?? null) === capeId) return;
  await wardrobeAction({ kind: 'cape', key: skin.texture_key }, 'Could not update the cape.', async () => {
    const updated = savedSkinRecord(
      await api('PUT', `/skins/${skin.texture_key}`, {
        name: skin.name,
        variant: skin.variant,
        cape_id: capeId,
      }),
    );
    if (!updated) throw new Error('Cape update returned an invalid response.');
    if (wardrobeSelection.value.kind !== 'saved') wardrobeSelection.value = { kind: 'saved', key: updated.texture_key };
    if (skin.applied_at && wardrobeContext.value.skinActionsEnabled) {
      return await applySavedSkin(updated.texture_key);
    }
    void refreshWardrobe();
    return 'Cape updated';
  });
}

export async function applyDefaultSkin(skin: DefaultSkin): Promise<void> {
  selectDefaultSkin(skin.id);
  const knownKey = await defaultSkinTextureKey(skin).catch(() => defaultSkinKeys.value.get(skin.id));
  if (knownKey) rememberDefaultSkinKey(skin.id, knownKey);
  const existing = knownKey ? (wardrobeData.value.skins.find((saved) => saved.texture_key === knownKey) ?? null) : null;
  await wardrobeAction({ kind: 'apply', key: knownKey ?? skin.id }, 'Could not apply skin.', async () => {
    if (existing) return await applySavedSkin(existing.texture_key, { select: false });
    const saved = await uploadSkinPng(await defaultSkinFile(skin), {
      name: skin.name,
      variant: skin.variant,
      source: DEFAULT_SKIN_SOURCE,
      applyAfterSave: true,
      select: false,
    });
    if (saved) rememberDefaultSkinKey(skin.id, saved.texture_key);
    return null;
  });
  selectDefaultSkin(skin.id);
}

export function seedProfileSkin(): void {
  const seed = currentProfileSeedCandidate();
  if (!seed) {
    invalidateProfileSeed();
    return;
  }
  if (profileSeedKey === seed.key) return;
  const requestId = ++profileSeedRequestId;
  profileSeedKey = seed.key;
  profileSavedKey.value = null;

  void api('POST', '/skins/from-profile', { variant: seed.variant, mark_current: true })
    .then((payload) => {
      if (
        profileSeedRequestId !== requestId ||
        profileSeedKey !== seed.key ||
        currentProfileSeedCandidate()?.key !== seed.key
      ) {
        return;
      }
      const saved = savedSkinRecord(payload);
      if (!saved) return;
      profileSavedKey.value = saved.texture_key;
      if (!hasSelectedSkinForAccount(seed.accountKey)) {
        wardrobeSelection.value = { kind: 'saved', key: saved.texture_key };
        setSelectedSkin(`saved:${saved.texture_key}`, seed.accountKey);
      }
      void refreshWardrobe();
    })
    .catch(() => {
      if (profileSeedRequestId === requestId && profileSeedKey === seed.key) profileSeedKey = null;
    });
}

export function inferredProfileSavedSkin(): SavedSkinRecord | null {
  if (!currentProfileSeedCandidate() || !profileSavedKey.value) return null;
  return wardrobeData.value.skins.find((skin) => skin.texture_key === profileSavedKey.value) ?? null;
}

export function wardrobeErrorMessage(err: unknown, fallback: string): string {
  return boundedMessage(err instanceof Error ? err.message : undefined, fallback);
}
