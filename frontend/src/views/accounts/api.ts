import { api, apiFetch, apiResourceUrl, apiUrl, isApiError } from '../../api';
import type { DefaultSkin } from '../../default-skins';
import type {
  AccountListResponse,
  MinecraftCape,
  MinecraftSkinLookup,
  PendingSkinStatus,
  SavedSkinRecord,
  SavedSkinSort,
  SavedSkinsResponse,
  SkinNormalizeResponse,
  SkinVariant,
  StagedSkinUpload,
  UploadSkinVariant,
} from './types';

export const DEFAULT_SKIN_SOURCE = 'minecraft_default_skin';

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/**
 * The local backend's responses use generated types. This only tells a payload
 * of the expected kind from an error body or an older build's shape.
 */
function wire<T>(value: unknown, key: keyof T & string, label: string): T {
  if (!isRecord(value) || !(key in value)) throw new Error(`${label} returned an invalid response.`);
  return value as T;
}

export function accountList(value: unknown): AccountListResponse {
  return wire<AccountListResponse>(value, 'accounts', 'Account list');
}

export function savedSkin(value: unknown, label = 'Saved skin'): SavedSkinRecord {
  return wire<SavedSkinRecord>(value, 'texture_key', label);
}

export function wardrobe(value: unknown): SavedSkinsResponse {
  return wire<SavedSkinsResponse>(value, 'skins', 'Saved skins');
}

export function pendingSkin(value: unknown): PendingSkinStatus {
  return wire<PendingSkinStatus>(value, 'generation', 'Skin change');
}

/** The sentence a command response asks to show, or the fallback. */
export function commandSummary(value: unknown, fallback: string): string {
  const view = isRecord(value) ? value.view_model : null;
  return isRecord(view) && typeof view.summary === 'string' && view.summary.trim() ? view.summary : fallback;
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

export function lookupCapeFileUrl(profile: MinecraftSkinLookup): string | undefined {
  if (!profile.cape_url) return undefined;
  const params = new URLSearchParams({ username: profile.username });
  return apiResourceUrl(`/skin/lookup/cape?${params.toString()}`);
}

export function lookupSkinFileUrl(profile: MinecraftSkinLookup): string {
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
      if (isRecord(reason) && reason.status === 'refused' && typeof reason.message === 'string') {
        throw new Error(reason.message);
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
export async function normalizeSkinUpload(file: Blob): Promise<SkinNormalizeResponse> {
  const response = await apiFetch(apiUrl('/skins/normalize'), {
    method: 'POST',
    headers: { 'Content-Type': 'image/png' },
    body: file,
  });
  const payload: unknown = await response.json().catch(() => undefined);
  if (!response.ok) {
    throw apiResponseError(response, payload, `Skin validation failed with HTTP ${response.status}`);
  }
  return wire<SkinNormalizeResponse>(payload, 'variant_suggestion', 'Skin validation');
}

export async function suggestedSavedSkinVariant(skin: SavedSkinRecord): Promise<SkinVariant> {
  return (await normalizeSkinUpload(await fetchSavedSkinPng(skin))).variant_suggestion;
}

export async function replaceSavedSkinTexture(
  textureKey: string,
  file: File,
  metadata: { name: string; variant: SkinVariant; capeId?: string | null },
): Promise<SavedSkinRecord> {
  const params = new URLSearchParams({
    name: metadata.name,
    variant: metadata.variant,
  });
  if (metadata.capeId !== undefined) {
    if (metadata.capeId) {
      params.set('cape_id', metadata.capeId);
    } else {
      params.set('clear_cape', 'true');
    }
  }
  const response = await apiFetch(apiUrl(`/skins/${encodeURIComponent(textureKey)}/texture?${params.toString()}`), {
    method: 'PUT',
    headers: { 'Content-Type': 'image/png' },
    body: file,
  });
  const payload: unknown = await response.json().catch(() => undefined);
  if (!response.ok) {
    throw apiResponseError(response, payload, `Texture replacement failed with HTTP ${response.status}`);
  }
  return savedSkin(payload, 'Texture replacement');
}

export function uploadSkinName(file: File): string {
  return file.name.replace(/\.[^.]+$/, '').trim();
}

/** Saves the named Minecraft player's skin to the library. */
export async function saveLookupSkin(username: string, variant: SkinVariant): Promise<SavedSkinRecord> {
  return savedSkin(await api('POST', '/skins/from-username', { username, variant }), 'Player skin save');
}

export async function lookupMinecraftSkin(username: string): Promise<MinecraftSkinLookup> {
  const payload = await api('GET', `/skin/lookup?${new URLSearchParams({ username })}`);
  return wire<MinecraftSkinLookup>(payload, 'texture_url', 'Minecraft skin lookup');
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

function readApiPayloadMessage(payload: unknown, fallback: string): string {
  if (isRecord(payload) && typeof payload.error === 'string' && payload.error.trim()) {
    return payload.error.trim();
  }
  return fallback;
}

export function apiResponseError(response: Response, payload: unknown, fallback: string): Error {
  const error = new Error(readApiPayloadMessage(payload, fallback)) as Error & {
    name: 'ApiError';
    status: number;
    statusText: string;
    payload?: unknown;
  };
  error.name = 'ApiError';
  error.status = response.status;
  error.statusText = response.statusText;
  if (payload !== undefined) error.payload = payload;
  return error;
}

export function skinActionErrorMessage(error: unknown, fallback: string): string {
  if (isApiError(error) && isRecord(error.payload)) {
    return boundedMessage(typeof error.payload.error === 'string' ? error.payload.error : undefined, fallback);
  }
  if (error instanceof Error) return boundedMessage(error.message, fallback);
  if (isRecord(error)) {
    return boundedMessage(typeof error.error === 'string' ? error.error : undefined, fallback);
  }
  return fallback;
}

export function savedSkinApplyErrorMessage(error: unknown): string {
  return skinActionErrorMessage(error, 'Minecraft profile apply failed.');
}

export function savedSkinFileUrl(skin: SavedSkinRecord): string {
  return apiResourceUrl(`/skins/${encodeURIComponent(skin.texture_key)}/file`);
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
