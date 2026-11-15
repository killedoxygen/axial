import { apiFetch, apiResourceUrl, isApiError, request, send } from '../platform/transport';
import type { DefaultSkin } from './default-skins';
import type { AccountCommandResponse } from '../generated/AccountCommandResponse';
import type { AccountListResponse } from '../generated/AccountListResponse';
import type { AccountRemovedResponse } from '../generated/AccountRemovedResponse';
import type { MinecraftCape } from '../generated/MinecraftCape';
import type { ProfileRefreshResponse } from '../generated/ProfileRefreshResponse';
import type { ProfileResetResponse } from '../generated/ProfileResetResponse';
import type { SavedSkinRecord } from '../generated/SavedSkinRecord';
import type { SavedSkinsResponse } from '../generated/SavedSkinsResponse';
import type { SkinApplyResponse } from '../generated/SkinApplyResponse';
import type { SkinCancelResponse } from '../generated/SkinCancelResponse';
import type { SkinFlushResponse } from '../generated/SkinFlushResponse';
import type { SkinNormalizeResponse } from '../generated/SkinNormalizeResponse';
import type { SkinVariant } from '../generated/SkinVariant';
import type { UsernameProfile } from '../generated/UsernameProfile';
import type { SavedSkinSort, StagedSkinUpload, UploadSkinVariant } from './types';

export const DEFAULT_SKIN_SOURCE = 'minecraft_default_skin';

/** The revisions a command expects, so a stale view cannot change a newer selection. */
export interface AccountFence {
  expected_account_revision: number;
  expected_selection_revision: number;
}

const account = (id: string): string => `/accounts/${encodeURIComponent(id)}`;
const skin = (key: string): string => `/skins/${encodeURIComponent(key)}`;

export function listAccounts(): Promise<AccountListResponse> {
  return request('GET', '/accounts');
}

export function selectAccount(id: string, fence: AccountFence): Promise<AccountCommandResponse> {
  return request('POST', `${account(id)}/select`, fence);
}

export function createOfflineAccount(
  username: string,
  fence: Pick<AccountFence, 'expected_selection_revision'>,
): Promise<AccountCommandResponse> {
  return request('POST', '/accounts/offline', { username, ...fence });
}

export function renameOfflineAccount(
  id: string,
  username: string,
  fence: AccountFence,
): Promise<AccountCommandResponse> {
  return request('PATCH', account(id), { username, ...fence });
}

export function removeAccount(id: string, fence: AccountFence): Promise<AccountRemovedResponse> {
  const query = new URLSearchParams({
    expected_account_revision: String(fence.expected_account_revision),
    expected_selection_revision: String(fence.expected_selection_revision),
  });
  return request('DELETE', `${account(id)}?${query}`);
}

export function refreshMicrosoftSignIn(): Promise<ProfileRefreshResponse> {
  return request('POST', '/auth/refresh');
}

export function syncMinecraftProfile(): Promise<ProfileRefreshResponse> {
  return request('POST', '/auth/profile/sync');
}

export function listSavedSkins(): Promise<SavedSkinsResponse> {
  return request('GET', '/skins');
}

export function applySavedSkinCommand(textureKey: string, query: URLSearchParams): Promise<SkinApplyResponse> {
  return request('POST', `${skin(textureKey)}/apply?${query}`);
}

export function flushPendingSkin(query: URLSearchParams): Promise<SkinFlushResponse> {
  return request('POST', `/skins/flush?${query}`);
}

export function cancelPendingSkin(query: URLSearchParams): Promise<SkinCancelResponse> {
  return request('DELETE', `/skins/pending?${query}`);
}

export function deleteSavedSkinRecord(textureKey: string): Promise<unknown> {
  return request('DELETE', skin(textureKey));
}

export function uploadSavedSkin(file: Blob, params: URLSearchParams): Promise<SavedSkinRecord> {
  return send('POST', `/skins?${params}`, file, 'image/png');
}

export function updateSavedSkin(
  textureKey: string,
  body: { name: string; variant: SkinVariant; cape_id?: string | null; clear_cape?: boolean },
): Promise<SavedSkinRecord> {
  return request('PUT', skin(textureKey), body);
}

/** Saves the selected account's current profile skin to the library. */
export function saveProfileSkin(query: URLSearchParams): Promise<SavedSkinRecord> {
  return request('POST', `/skins/from-profile?${query}`, { mark_current: true });
}

export function resetProfileSkinCommand(query: URLSearchParams): Promise<ProfileResetResponse> {
  return request('POST', `/skin/profile/reset?${query}`, {});
}

export function resetProfileCapeCommand(query: URLSearchParams): Promise<ProfileResetResponse> {
  return request('POST', `/skin/cape/reset?${query}`, {});
}

export function stagedSkinVariant(staged: StagedSkinUpload, selectedVariant: UploadSkinVariant): SkinVariant {
  return selectedVariant === 'auto' ? staged.detectedVariant : selectedVariant;
}

export function stagedSkinPreviewSrc(staged: StagedSkinUpload): string {
  return staged.normalizedDataUrl || staged.objectUrl;
}

export function capeFileUrl(cape: MinecraftCape): string {
  return apiResourceUrl(`/skin/cape/file?id=${encodeURIComponent(cape.id)}`);
}

export function lookupCapeFileUrl(profile: UsernameProfile): string | undefined {
  if (!profile.cape_url) return undefined;
  const params = new URLSearchParams({ username: profile.username });
  return apiResourceUrl(`/skin/lookup/cape?${params.toString()}`);
}

export function lookupSkinFileUrl(profile: UsernameProfile): string {
  return apiResourceUrl(`/skin/lookup/file?${new URLSearchParams({ username: profile.username })}`);
}

export function isPngFile(file: File): boolean {
  const type = file.type.trim().toLowerCase();
  if (type) return type === 'image/png';
  return file.name.toLowerCase().endsWith('.png');
}

export async function fetchSavedSkinPng(skin: SavedSkinRecord): Promise<Blob> {
  const response = await apiFetch(savedSkinFileUrl(skin), { cache: 'no-store' });
  if (!response.ok) {
    throw new Error(`Saved skin PNG download failed with HTTP ${response.status}.`);
  }
  const blob = await response.blob();
  if (blob.size < 1) {
    throw new Error('Saved skin PNG was empty.');
  }
  return blob;
}

export async function exportNativeSavedSkinPng(skin: SavedSkinRecord): Promise<'published' | 'cancelled' | undefined> {
  const tauri = typeof window === 'undefined' ? undefined : window.__TAURI__;
  if (!tauri) return undefined;
  const unknown = 'Skin PNG export outcome is unknown. Check the chosen destination before trying again.';
  if (!tauri.core) throw new Error(unknown);
  const payload = await tauri.core
    .invoke('export_saved_skin_png', { expected: { ...skin } })
    .catch((reason: unknown) => {
      const refusal = reason as { status?: unknown; message?: unknown } | null;
      if (refusal?.status === 'refused' && typeof refusal.message === 'string') {
        throw new Error(refusal.message);
      }
      throw new Error(unknown);
    });
  if (payload !== 'published' && payload !== 'cancelled') throw new Error(unknown);
  return payload;
}

export function downloadBlob(blob: Blob, filename: string): void {
  const objectUrl = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = objectUrl;
  anchor.download = filename;
  anchor.style.display = 'none';
  document.body.append(anchor);
  anchor.click();
  anchor.remove();
  window.setTimeout(() => URL.revokeObjectURL(objectUrl), 30_000);
}

/** Checks a skin PNG and returns its normalized form and the model the backend suggests for it. */
export function normalizeSkinUpload(file: Blob): Promise<SkinNormalizeResponse> {
  return send('POST', '/skins/normalize', file, 'image/png');
}

export async function suggestedSavedSkinVariant(skin: SavedSkinRecord): Promise<SkinVariant> {
  return (await normalizeSkinUpload(await fetchSavedSkinPng(skin))).variant_suggestion;
}

export function replaceSavedSkinTexture(
  textureKey: string,
  file: File,
  metadata: { name: string; variant: SkinVariant; capeId?: string | null },
): Promise<SavedSkinRecord> {
  const params = new URLSearchParams({ name: metadata.name, variant: metadata.variant });
  if (metadata.capeId !== undefined) {
    if (metadata.capeId) params.set('cape_id', metadata.capeId);
    else params.set('clear_cape', 'true');
  }
  return send('PUT', `${skin(textureKey)}/texture?${params}`, file, 'image/png');
}

export function uploadSkinName(file: File): string {
  return file.name.replace(/\.[^.]+$/, '').trim();
}

/** Saves the named Minecraft player's skin to the library. */
export function saveLookupSkin(username: string, variant: SkinVariant): Promise<SavedSkinRecord> {
  return request('POST', '/skins/from-username', { username, variant });
}

export function lookupMinecraftSkin(username: string): Promise<UsernameProfile> {
  return request('GET', `/skin/lookup?${new URLSearchParams({ username })}`);
}

export function boundedMessage(value: string | undefined, fallback: string): string {
  const trimmed = value?.trim();
  if (!trimmed) return fallback;
  return trimmed.length > 180 ? `${trimmed.slice(0, 177)}...` : trimmed;
}

export function savedSkinSourceLabel(source: string): string {
  switch (source) {
    case DEFAULT_SKIN_SOURCE:
      return 'Minecraft default';
    case 'minecraft_profile_skin':
      return 'Minecraft profile';
    case 'minecraft_username_skin':
      return 'Player lookup';
    case 'local_upload':
      return 'Upload';
    default:
      return 'Saved skin';
  }
}

function savedSkinRecentTime(skin: SavedSkinRecord): number {
  const updatedAt = Date.parse(skin.updated_at);
  if (Number.isFinite(updatedAt)) return updatedAt;
  const createdAt = Date.parse(skin.created_at);
  return Number.isFinite(createdAt) ? createdAt : 0;
}

function savedSkinCompareByName(left: SavedSkinRecord, right: SavedSkinRecord): number {
  return left.name.localeCompare(right.name, undefined, { numeric: true, sensitivity: 'base' });
}

export function sortSavedSkins(skins: SavedSkinRecord[], sort: SavedSkinSort): SavedSkinRecord[] {
  return skins
    .map((skin, index) => ({ skin, index }))
    .sort((left, right) => {
      const nameCompare = savedSkinCompareByName(left.skin, right.skin);
      const recentCompare = savedSkinRecentTime(right.skin) - savedSkinRecentTime(left.skin);

      let result = 0;
      if (sort === 'name') {
        result = nameCompare || recentCompare;
      } else if (sort === 'equipped') {
        result =
          Number(Boolean(right.skin.applied_at)) - Number(Boolean(left.skin.applied_at)) ||
          recentCompare ||
          nameCompare;
      } else if (sort === 'source') {
        result =
          savedSkinSourceLabel(left.skin.source).localeCompare(savedSkinSourceLabel(right.skin.source), undefined, {
            numeric: true,
            sensitivity: 'base',
          }) ||
          nameCompare ||
          recentCompare;
      } else {
        result = recentCompare || nameCompare;
      }

      return result || left.index - right.index;
    })
    .map(({ skin }) => skin);
}

export function skinActionErrorMessage(error: unknown, fallback: string): string {
  if (isApiError(error) || error instanceof Error) return boundedMessage(error.message, fallback);
  return fallback;
}

export function savedSkinApplyErrorMessage(error: unknown): string {
  return skinActionErrorMessage(error, 'Minecraft profile apply failed.');
}

export function savedSkinFileUrl(saved: Pick<SavedSkinRecord, 'texture_key'>): string {
  return apiResourceUrl(`${skin(saved.texture_key)}/file`);
}

/** The selected account's current profile skin, optionally pinned to a revision for caching. */
export function profileSkinFileUrl(params?: URLSearchParams): string {
  return apiResourceUrl(params ? `/skin/profile/file?${params}` : '/skin/profile/file');
}

export async function defaultSkinFile(skin: DefaultSkin): Promise<File> {
  const response = await fetch(skin.src);
  const blob = await response.blob();
  return new File([blob], `${skin.name}.png`, { type: 'image/png' });
}

export function savedSkinDownloadFilename(skin: SavedSkinRecord): string {
  const name = skin.name
    .trim()
    .replace(/[\\/:*?"<>|\x00-\x1f]+/g, ' ')
    .replace(/\s+/g, ' ')
    .slice(0, 80)
    .trim();
  const baseName =
    name
      .replace(/\.png$/i, '')
      .replace(/[. ]+$/g, '')
      .trim() || 'saved-skin';
  return `${baseName}.png`;
}
