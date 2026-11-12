import { signal } from '@preact/signals';
import { api, apiFetch, apiUrl } from '../api';
import { DEFAULT_SKINS, type DefaultSkin } from '../default-skins';
import {
  FALLBACK_SKIN_ACCOUNT_KEY,
  hasSelectedSkinForAccount,
  launcherSkinAccountKey,
  resetSelectedSkin,
  selectedSkinForAccount,
  setSelectedSkin,
} from '../player-skin';
import { toast } from '../toast';
import { showConfirm } from '../ui/Dialog';
import {
  apiResponseError,
  boundedMessage,
  commandSummary,
  DEFAULT_SKIN_SOURCE,
  defaultSkinFile,
  downloadBlob,
  exportNativeSavedSkinPng,
  fetchSavedSkinPng,
  isRecord,
  pendingSkin,
  savedSkin,
  savedSkinApplyErrorMessage,
  savedSkinDownloadFilename,
  skinActionErrorMessage,
  wardrobe,
} from '../views/accounts/api';
import type { LauncherAccount, PendingSkinStatus, SavedSkinRecord, SkinVariant } from '../views/accounts/types';
import { refreshAccountsData } from './accounts';

export type WardrobeSelection =
  | { kind: 'saved'; key: string }
  | { kind: 'default'; id: string }
  | { kind: 'profile' }
  | { kind: 'lookup' }
  | { kind: 'none' };

export interface WardrobeContext {
  accountKey: string;
  selectionRevision: number | null;
  skinActionsEnabled: boolean;
  /** The selected Microsoft account, whose Minecraft profile the wardrobe changes. */
  account: LauncherAccount | null;
}

export interface WardrobeCapture {
  context: WardrobeContext;
  revision: number;
}

export interface WardrobeData {
  state: 'loading' | 'ready' | 'unavailable';
  skins: SavedSkinRecord[];
  pendingApply: PendingSkinStatus | null;
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

/** Time the backend takes to apply a skin once its queue delay has passed. */
const APPLY_SETTLE_MS = 1_500;
const DEFERRED_APPLY_READ_RETRIES = 3;

export const wardrobeContext = signal<WardrobeContext>({
  accountKey: 'account:fallback',
  selectionRevision: null,
  skinActionsEnabled: false,
  account: null,
});
export const wardrobeData = signal<WardrobeData>({ state: 'loading', skins: [], pendingApply: null, error: null });
export const wardrobeSelection = signal<WardrobeSelection>({ kind: 'none' });
export const wardrobeOp = signal<WardrobeOp | null>(null);
export const wardrobeNotice = signal<string | null>(null);

let wardrobeRequestId = 0;
let wardrobeContextRevision = 0;
let deferredApplyTimer: number | null = null;
let profileSeedRequestId = 0;
let profileSeedKey: string | null = null;
const profileSavedKey = signal<string | null>(null);

export function setWardrobeNotice(text: string | null): void {
  wardrobeNotice.value = text;
}

export function wardrobeBusy(): boolean {
  return wardrobeOp.value !== null;
}

export function captureWardrobeContext(): WardrobeCapture {
  return { context: wardrobeContext.value, revision: wardrobeContextRevision };
}

export function isWardrobeContextCurrent(capture: WardrobeCapture): boolean {
  return capture.revision === wardrobeContextRevision;
}

function requireCurrentWardrobeContext(capture: WardrobeCapture): void {
  if (!isWardrobeContextCurrent(capture))
    throw new Error('The account changed. Select the skin again before applying it.');
}

function skinCommandQuery(capture: WardrobeCapture, generation?: number): URLSearchParams {
  requireCurrentWardrobeContext(capture);
  const { accountKey, selectionRevision } = capture.context;
  if (selectionRevision === null || accountKey === FALLBACK_SKIN_ACCOUNT_KEY || !accountKey.startsWith('account:')) {
    throw new Error('Refresh the selected account before changing its skin.');
  }
  const query = new URLSearchParams({
    expected_account_id: accountKey.slice('account:'.length),
    expected_selection_revision: String(selectionRevision),
  });
  if (generation !== undefined) query.set('expected_generation', String(generation));
  return query;
}

function pendingSkinCommandQuery(capture: WardrobeCapture): URLSearchParams {
  const pending = wardrobeData.value.pendingApply;
  if (
    !isPendingActive(pending) ||
    launcherSkinAccountKey(pending.account_id) !== capture.context.accountKey
  ) {
    throw new Error('Refresh the queued skin change before trying again.');
  }
  return skinCommandQuery(capture, pending.generation);
}

function profileSkinKey(account: LauncherAccount | null): string | null {
  const skin = account?.profile_skin;
  return skin ? JSON.stringify([account.account_id, skin.id, skin.url, skin.variant]) : null;
}

function isPendingActive(pending: PendingSkinStatus | null): pending is PendingSkinStatus {
  return pending?.phase === 'queued' || pending?.phase === 'applying';
}

/** The saved copy of a built-in skin, which keeps the name and source it was saved with. */
export function savedDefaultSkin(skin: DefaultSkin, skins = wardrobeData.value.skins): SavedSkinRecord | null {
  return (
    skins.find(
      (saved) => saved.source === DEFAULT_SKIN_SOURCE && saved.name === skin.name && saved.variant === skin.variant,
    ) ?? null
  );
}

export function setWardrobeContext(next: WardrobeContext): void {
  const current = wardrobeContext.value;
  const accountChanged = current.accountKey !== next.accountKey || current.selectionRevision !== next.selectionRevision;
  const changed =
    accountChanged ||
    current.skinActionsEnabled !== next.skinActionsEnabled ||
    profileSkinKey(current.account) !== profileSkinKey(next.account);
  wardrobeContext.value = next;
  if (changed) wardrobeContextRevision += 1;
  if (accountChanged) {
    wardrobeRequestId += 1;
    wardrobeSelection.value = { kind: 'none' };
    wardrobeNotice.value = null;
    wardrobeData.value = { ...wardrobeData.value, state: 'loading', pendingApply: null, error: null };
    schedulePendingApplyRecheck(null);
  }
  if (changed) void refreshWardrobe();
  if (changed) reconcileWardrobeSelection();
  seedProfileSkin();
}

export function refreshWardrobe(): Promise<void> {
  return readWardrobe(0);
}

async function readWardrobe(readFailures: number): Promise<void> {
  const requestId = ++wardrobeRequestId;
  const capture = captureWardrobeContext();
  try {
    const { skins, pending } = wardrobe(await api('GET', '/skins'));
    if (requestId !== wardrobeRequestId || !isWardrobeContextCurrent(capture)) return;
    // The backend answers for the account selected there; a different one means ours is stale.
    if (pending && launcherSkinAccountKey(pending.account_id) !== capture.context.accountKey) {
      void refreshAccountsData();
      return;
    }
    wardrobeData.value = { state: 'ready', skins, pendingApply: pending, error: null };
    schedulePendingApplyRecheck(pending);
  } catch (err: unknown) {
    if (requestId !== wardrobeRequestId || !isWardrobeContextCurrent(capture)) return;
    const previous = wardrobeData.value;
    const error = skinActionErrorMessage(err, 'Saved skins are unavailable.');
    wardrobeData.value =
      previous.state === 'ready'
        ? { ...previous, error }
        : { state: 'unavailable', skins: [], pendingApply: null, error };
    schedulePendingApplyRecheck(
      readFailures < DEFERRED_APPLY_READ_RETRIES ? previous.pendingApply : null,
      readFailures + 1,
    );
  }
  reconcileWardrobeSelection();
  seedProfileSkin();
}

function schedulePendingApplyRecheck(pending: PendingSkinStatus | null, readFailures = 0): void {
  if (deferredApplyTimer !== null) {
    window.clearTimeout(deferredApplyTimer);
    deferredApplyTimer = null;
  }
  if (!isPendingActive(pending)) return;
  const capture = captureWardrobeContext();
  deferredApplyTimer = window.setTimeout(() => {
    deferredApplyTimer = null;
    if (!isWardrobeContextCurrent(capture)) return;
    void readWardrobe(readFailures).then(() => {
      if (isWardrobeContextCurrent(capture)) void refreshAccountsData();
    });
  }, (pending.due_in_ms ?? 0) + APPLY_SETTLE_MS);
}

function currentProfileSeedCandidate(): { key: string; accountKey: string } | null {
  const { accountKey, skinActionsEnabled, account } = wardrobeContext.value;
  const key = profileSkinKey(account);
  if (wardrobeData.value.state !== 'ready' || !skinActionsEnabled || !key) return null;
  return { key, accountKey };
}

function invalidateProfileSeed(): void {
  if (profileSeedKey !== null) profileSeedRequestId += 1;
  profileSeedKey = null;
  profileSavedKey.value = null;
}

export function reconcileWardrobeSelection(): void {
  const { accountKey, skinActionsEnabled, account } = wardrobeContext.value;
  const data = wardrobeData.value;
  const current = wardrobeSelection.value;
  const profileSkin = account?.profile_skin;

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
  const capture = captureWardrobeContext();
  try {
    const summary = await task();
    if (summary && isWardrobeContextCurrent(capture)) toast(summary);
    return true;
  } catch (err: unknown) {
    if (isWardrobeContextCurrent(capture)) wardrobeNotice.value = skinActionErrorMessage(err, fallbackError);
    return false;
  } finally {
    wardrobeOp.value = null;
  }
}

export async function applySavedSkin(
  textureKey: string,
  options: { select?: boolean; capture?: WardrobeCapture } = {},
): Promise<string> {
  const capture = options.capture ?? captureWardrobeContext();
  requireCurrentWardrobeContext(capture);
  if (!capture.context.skinActionsEnabled) throw new Error('Online Minecraft account required.');
  const query = skinCommandQuery(capture);
  query.set('defer', 'true');
  const response = await api('POST', `/skins/${encodeURIComponent(textureKey)}/apply?${query.toString()}`);
  requireCurrentWardrobeContext(capture);
  const accepted = pendingSkin(isRecord(response) ? response.pending : undefined);
  if (
    !isPendingActive(accepted) ||
    launcherSkinAccountKey(accepted.account_id) !== capture.context.accountKey ||
    accepted.texture_key !== textureKey
  ) {
    throw new Error('Skin apply returned an invalid queued change. Refresh the skin library to check its status.');
  }
  wardrobeData.value = { ...wardrobeData.value, pendingApply: accepted, error: null };
  if (options.select !== false) selectSavedSkin(textureKey);
  await refreshWardrobe();
  return commandSummary(response, 'Skin command accepted.');
}

export async function applySkin(textureKey: string): Promise<void> {
  const skin = wardrobeData.value.skins.find((saved) => saved.texture_key === textureKey);
  if (skin?.applied_at) return;
  await wardrobeAction({ kind: 'apply', key: textureKey }, 'Could not apply skin.', () => applySavedSkin(textureKey));
}

export async function flushPendingApply(): Promise<void> {
  const capture = captureWardrobeContext();
  await wardrobeAction({ kind: 'flush' }, 'Could not apply queued skin.', async () => {
    const query = pendingSkinCommandQuery(capture);
    const response = await api('POST', `/skins/flush?${query.toString()}`);
    if (!isWardrobeContextCurrent(capture)) return null;
    await refreshWardrobe();
    void refreshAccountsData();
    return commandSummary(response, 'Skin command accepted.');
  });
}

export async function cancelPendingApply(): Promise<void> {
  const capture = captureWardrobeContext();
  await wardrobeAction({ kind: 'cancel-pending' }, 'Could not cancel queued skin apply.', async () => {
    const query = pendingSkinCommandQuery(capture);
    const response = await api('DELETE', `/skins/pending?${query.toString()}`);
    if (!isWardrobeContextCurrent(capture)) return null;
    schedulePendingApplyRecheck(null);
    wardrobeData.value = { ...wardrobeData.value, pendingApply: null, error: null };
    await refreshWardrobe();
    return commandSummary(response, 'Skin change canceled.');
  });
}

export async function deleteSavedSkin(skin: SavedSkinRecord): Promise<void> {
  const capture = captureWardrobeContext();
  const name = skin.name.trim();
  const ok = await showConfirm(
    name
      ? `Delete saved skin "${name}"? This removes it from local saved skins only.`
      : 'Delete this saved skin? This removes it from local saved skins only.',
    { title: 'Delete saved skin', destructive: true, confirmText: 'Delete' },
  );
  if (!ok) return;
  await wardrobeAction({ kind: 'delete', key: skin.texture_key }, 'Could not delete skin.', async () => {
    await api('DELETE', `/skins/${encodeURIComponent(skin.texture_key)}`);
    if (!isWardrobeContextCurrent(capture)) {
      void refreshWardrobe();
      return null;
    }
    const accountKey = capture.context.accountKey;
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
    const native = await exportNativeSavedSkinPng(skin);
    if (native === 'cancelled') return null;
    if (native === undefined) downloadBlob(await fetchSavedSkinPng(skin), savedSkinDownloadFilename(skin));
    return 'Skin PNG downloaded';
  });
}

export async function uploadSkinPng(
  file: File,
  options: {
    name: string;
    /** Left out, the backend picks the model the texture suggests. */
    variant?: SkinVariant;
    capeId?: string | null;
    source?: string;
    applyAfterSave: boolean;
    select?: boolean;
    capture?: WardrobeCapture;
  },
): Promise<SavedSkinRecord | null> {
  const capture = options.capture ?? captureWardrobeContext();
  const params = new URLSearchParams({ name: options.name });
  if (options.variant) params.set('variant', options.variant);
  if (options.capeId) params.set('cape_id', options.capeId);
  if (options.source) params.set('source', options.source);
  const response = await apiFetch(apiUrl(`/skins?${params.toString()}`), {
    method: 'POST',
    headers: { 'Content-Type': 'image/png' },
    body: file,
  });
  const payload: unknown = await response.json().catch(() => undefined);
  if (!response.ok) throw apiResponseError(response, payload, `Upload failed with HTTP ${response.status}`);
  const saved = savedSkin(payload, 'Skin upload');
  if (!isWardrobeContextCurrent(capture)) {
    void refreshWardrobe();
    return saved;
  }
  if (options.select !== false) selectSavedSkin(saved.texture_key);
  if (options.applyAfterSave) {
    try {
      toast(await applySavedSkin(saved.texture_key, { select: options.select !== false, capture }));
    } catch (err: unknown) {
      void refreshWardrobe();
      if (isWardrobeContextCurrent(capture)) wardrobeNotice.value = savedSkinApplyErrorMessage(err);
    }
  } else {
    void refreshWardrobe();
    if (options.source !== DEFAULT_SKIN_SOURCE) toast('Skin added to your library');
  }
  return saved;
}

export async function saveProfileSkinLocally(): Promise<void> {
  const capture = captureWardrobeContext();
  if (!wardrobeContext.value.skinActionsEnabled) return;
  await wardrobeAction({ kind: 'save-profile' }, 'Could not save Minecraft profile skin.', async () => {
    const query = skinCommandQuery(capture);
    const saved = savedSkin(
      await api('POST', `/skins/from-profile?${query.toString()}`, { mark_current: true }),
      'Profile skin save',
    );
    if (!isWardrobeContextCurrent(capture)) return null;
    profileSavedKey.value = saved.texture_key;
    selectSavedSkin(saved.texture_key);
    void refreshWardrobe();
    return 'Profile skin added to your library';
  });
}

export async function resetProfileSkin(): Promise<void> {
  const capture = captureWardrobeContext();
  const { skinActionsEnabled, account } = wardrobeContext.value;
  if (!skinActionsEnabled || !account?.profile_skin) return;
  const ok = await showConfirm(
    'Reset the active Minecraft profile skin to the default skin? Axial will save the current profile skin locally first.',
    { title: 'Reset profile skin', destructive: true, confirmText: 'Reset' },
  );
  if (!ok) return;
  await wardrobeAction({ kind: 'reset-profile-skin' }, 'Could not reset Minecraft profile skin.', async () => {
    const query = skinCommandQuery(capture);
    const response = await api('POST', `/skin/profile/reset?${query.toString()}`, {});
    void refreshWardrobe();
    void refreshAccountsData();
    return commandSummary(response, 'Skin command accepted.');
  });
}

export async function resetProfileCape(): Promise<void> {
  const capture = captureWardrobeContext();
  if (!wardrobeContext.value.skinActionsEnabled) return;
  const ok = await showConfirm(
    'Remove the active Minecraft profile cape? Axial will save the current skin and cape pairing locally first.',
    { title: 'Reset profile cape', destructive: true, confirmText: 'Reset cape' },
  );
  if (!ok) return;
  await wardrobeAction({ kind: 'reset-profile-cape' }, 'Could not reset Minecraft profile cape.', async () => {
    const query = skinCommandQuery(capture);
    const response = await api('POST', `/skin/cape/reset?${query.toString()}`, {});
    void refreshWardrobe();
    void refreshAccountsData();
    return commandSummary(response, 'Skin command accepted.');
  });
}

export async function changeSavedSkinCape(skin: SavedSkinRecord, capeId: string | null): Promise<void> {
  const capture = captureWardrobeContext();
  if ((skin.cape_id ?? null) === capeId) return;
  await wardrobeAction({ kind: 'cape', key: skin.texture_key }, 'Could not update the cape.', async () => {
    const updated = savedSkin(
      await api('PUT', `/skins/${encodeURIComponent(skin.texture_key)}`, {
        name: skin.name,
        variant: skin.variant,
        cape_id: capeId,
      }),
      'Cape update',
    );
    if (!isWardrobeContextCurrent(capture)) {
      void refreshWardrobe();
      return null;
    }
    if (wardrobeSelection.value.kind !== 'saved') wardrobeSelection.value = { kind: 'saved', key: updated.texture_key };
    if (skin.applied_at && wardrobeContext.value.skinActionsEnabled) {
      return await applySavedSkin(updated.texture_key, { capture });
    }
    void refreshWardrobe();
    return 'Cape updated';
  });
}

export async function applyDefaultSkin(skin: DefaultSkin): Promise<void> {
  if (wardrobeBusy()) return;
  const capture = captureWardrobeContext();
  selectDefaultSkin(skin.id);
  await wardrobeAction({ kind: 'apply', key: skin.id }, 'Could not apply skin.', async () => {
    const existing = savedDefaultSkin(skin);
    if (existing) return await applySavedSkin(existing.texture_key, { select: false, capture });
    await uploadSkinPng(await defaultSkinFile(skin), {
      name: skin.name,
      variant: skin.variant,
      source: DEFAULT_SKIN_SOURCE,
      applyAfterSave: true,
      select: false,
      capture,
    });
    return null;
  });
  if (isWardrobeContextCurrent(capture)) selectDefaultSkin(skin.id);
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

  const capture = captureWardrobeContext();
  void (async () => {
    const query = skinCommandQuery(capture);
    return api('POST', `/skins/from-profile?${query.toString()}`, { mark_current: true });
  })()
    .then((payload) => {
      if (
        profileSeedRequestId !== requestId ||
        profileSeedKey !== seed.key ||
        currentProfileSeedCandidate()?.key !== seed.key
      ) {
        return;
      }
      const saved = savedSkin(payload);
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
