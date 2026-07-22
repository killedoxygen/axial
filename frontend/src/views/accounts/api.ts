import { api, apiResourceUrl, apiUrl, isApiError } from '../../api';
import { DEFAULT_SKINS, type DefaultSkin } from '../../default-skins';
import type { NativeDragDropPayload } from '../../native';
import type {
  AccountActionState,
  AuthStatusRecord,
  CommandViewModel,
  LauncherAccount,
  LauncherAccountsData,
  MinecraftAuthReadiness,
  MinecraftCape,
  MinecraftProfile,
  MinecraftSkin,
  MinecraftSkinLookup,
  SavedSkinRecord,
  SavedSkinSort,
  SavedSkinsData,
  SkinFlushResult,
  SkinNormalizeMetadata,
  SkinVariant,
  StagedSkinUpload,
  UploadSkinVariant,
} from './types';

export const DEFAULT_SKIN_SOURCE = 'minecraft_default_skin';

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

export function maybeNumber(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function minecraftSkin(value: unknown): MinecraftSkin | null {
  if (!isRecord(value)) return null;
  if (
    typeof value.id !== 'string' ||
    typeof value.state !== 'string' ||
    typeof value.url !== 'string' ||
    typeof value.variant !== 'string'
  ) {
    return null;
  }

  return {
    id: value.id,
    state: value.state,
    url: value.url,
    variant: value.variant,
  };
}

function minecraftCape(value: unknown): MinecraftCape | null {
  if (!isRecord(value)) return null;
  if (typeof value.id !== 'string' || typeof value.state !== 'string' || typeof value.url !== 'string') {
    return null;
  }

  return {
    id: value.id,
    state: value.state,
    url: value.url,
  };
}

export function savedSkinRecord(value: unknown): SavedSkinRecord | null {
  if (!isRecord(value)) return null;
  if (
    typeof value.texture_key !== 'string' ||
    typeof value.name !== 'string' ||
    (value.variant !== 'classic' && value.variant !== 'slim') ||
    typeof value.source !== 'string' ||
    (value.cape_id !== undefined && value.cape_id !== null && typeof value.cape_id !== 'string') ||
    typeof value.created_at !== 'string' ||
    typeof value.updated_at !== 'string' ||
    (value.applied_at !== undefined && value.applied_at !== null && typeof value.applied_at !== 'string') ||
    typeof value.byte_size !== 'number'
  ) {
    return null;
  }

  return {
    texture_key: value.texture_key,
    name: value.name,
    variant: value.variant,
    source: value.source,
    cape_id: typeof value.cape_id === 'string' ? value.cape_id : null,
    created_at: value.created_at,
    updated_at: value.updated_at,
    applied_at: typeof value.applied_at === 'string' ? value.applied_at : null,
    byte_size: value.byte_size,
  };
}

export function savedSkinsResponse(value: unknown): SavedSkinsData | null {
  if (!isRecord(value) || !Array.isArray(value.skins)) return null;
  if (
    value.pending_apply_texture_key !== undefined &&
    value.pending_apply_texture_key !== null &&
    typeof value.pending_apply_texture_key !== 'string'
  ) {
    return null;
  }
  return {
    skins: value.skins.map(savedSkinRecord).filter((skin): skin is SavedSkinRecord => Boolean(skin)),
    pendingApplyKey: typeof value.pending_apply_texture_key === 'string' ? value.pending_apply_texture_key : null,
  };
}

export function commandViewModel(value: unknown): CommandViewModel | undefined {
  if (!isRecord(value) || typeof value.summary !== 'string') return undefined;
  return {
    summary: value.summary,
    detail: typeof value.detail === 'string' ? value.detail : undefined,
  };
}

export function commandSummary(value: unknown, fallback: string): string {
  if (isRecord(value)) {
    const view = commandViewModel(value.view_model);
    if (view?.summary.trim()) return view.summary;
  }
  return fallback;
}

function skinNormalizeMetadata(value: unknown): SkinNormalizeMetadata | null {
  if (!isRecord(value)) return null;
  if (
    typeof value.texture_key !== 'string' ||
    (value.variant_suggestion !== 'classic' && value.variant_suggestion !== 'slim') ||
    (value.normalized_data_url !== undefined && typeof value.normalized_data_url !== 'string')
  ) {
    return null;
  }

  return {
    textureKey: value.texture_key,
    variantSuggestion: value.variant_suggestion,
    normalizedDataUrl:
      typeof value.normalized_data_url === 'string' && value.normalized_data_url.startsWith('data:image/png;base64,')
        ? value.normalized_data_url
        : undefined,
  };
}

export function skinFlushResult(value: unknown): SkinFlushResult | null {
  if (!isRecord(value) || typeof value.status !== 'string' || typeof value.applied !== 'number') {
    return null;
  }
  return {
    status: value.status,
    applied: value.applied,
    viewModel: commandViewModel(value.view_model),
  };
}

export function skinVariantValue(value: string | undefined): SkinVariant {
  return value?.toLowerCase() === 'slim' ? 'slim' : 'classic';
}

export function stagedSkinVariant(staged: StagedSkinUpload, selectedVariant: UploadSkinVariant): SkinVariant {
  return selectedVariant === 'auto' ? staged.detectedVariant : selectedVariant;
}

export function stagedSkinPreviewSrc(staged: StagedSkinUpload): string {
  return staged.normalizedDataUrl || staged.objectUrl;
}

export function activeMinecraftSkin(profile: MinecraftProfile | undefined): MinecraftSkin | null {
  if (!profile) return null;
  return profile.skins.find((skin) => skin.state.toLowerCase() === 'active') ?? profile.skins[0] ?? null;
}

export function activeMinecraftCape(profile: MinecraftProfile | undefined): MinecraftCape | null {
  if (!profile) return null;
  return profile.capes.find((cape) => cape.state.toLowerCase() === 'active') ?? null;
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
  return apiResourceUrl(profile.texture_file_url);
}

export function isPngFile(file: File): boolean {
  const type = file.type.trim().toLowerCase();
  if (type) return type === 'image/png';
  return file.name.toLowerCase().endsWith('.png');
}

function cssPointFromNativeDrag(position: NativeDragDropPayload['position']): { x: number; y: number } | null {
  if (!position) return null;
  const pixelRatio = window.devicePixelRatio || 1;
  if (pixelRatio > 1 && (position.x > window.innerWidth + 1 || position.y > window.innerHeight + 1)) {
    return {
      x: position.x / pixelRatio,
      y: position.y / pixelRatio,
    };
  }
  return position;
}

export function nativeDragPositionHitsElement(
  position: NativeDragDropPayload['position'],
  element: HTMLElement | null,
): boolean {
  const point = cssPointFromNativeDrag(position);
  if (!point || !element) return false;
  const rect = element.getBoundingClientRect();
  return point.x >= rect.left && point.x <= rect.right && point.y >= rect.top && point.y <= rect.bottom;
}

export function nativeDragTargetElement<T extends HTMLElement>(
  position: NativeDragDropPayload['position'],
  selector: string,
): T | null {
  const point = cssPointFromNativeDrag(position);
  if (!point) return null;

  for (const element of document.elementsFromPoint(point.x, point.y)) {
    if (!(element instanceof HTMLElement)) continue;
    const match = element.closest(selector);
    if (match instanceof HTMLElement) return match as T;
  }

  for (const element of Array.from(document.querySelectorAll<HTMLElement>(selector))) {
    const rect = element.getBoundingClientRect();
    if (point.x >= rect.left && point.x <= rect.right && point.y >= rect.top && point.y <= rect.bottom) {
      return element as T;
    }
  }

  return null;
}

function loadSkinImage(url: string): Promise<HTMLImageElement> {
  return new Promise<HTMLImageElement>((resolve, reject) => {
    const next = new Image();
    next.onload = () => resolve(next);
    next.onerror = () => reject(new Error('Could not inspect skin image.'));
    next.src = url;
  });
}

function inferSkinVariantFromImage(image: HTMLImageElement): SkinVariant {
  if (image.width < 64 || image.height < 64) return 'classic';

  const canvas = document.createElement('canvas');
  canvas.width = image.width;
  canvas.height = image.height;
  const context = canvas.getContext('2d', { willReadFrequently: true });
  if (!context) throw new Error('Could not inspect skin image.');

  context.drawImage(image, 0, 0);
  const armAlpha = context.getImageData(54, 20, 2, 12).data;
  for (let index = 3; index < armAlpha.length; index += 4) {
    if (armAlpha[index] !== 0) return 'classic';
  }
  return 'slim';
}

async function detectSkinVariantFromPng(file: File): Promise<SkinVariant> {
  const url = URL.createObjectURL(file);
  try {
    return inferSkinVariantFromImage(await loadSkinImage(url));
  } catch {
    return 'classic';
  } finally {
    URL.revokeObjectURL(url);
  }
}

export async function detectSkinVariantFromSavedSkin(skin: SavedSkinRecord): Promise<SkinVariant> {
  const response = await fetch(savedSkinFileUrl(skin));
  if (!response.ok) {
    throw new Error(`Could not load saved skin PNG (${response.status}).`);
  }

  const blob = await response.blob();
  const url = URL.createObjectURL(blob);
  try {
    return inferSkinVariantFromImage(await loadSkinImage(url));
  } finally {
    URL.revokeObjectURL(url);
  }
}

export async function fetchSavedSkinPng(skin: SavedSkinRecord): Promise<Blob> {
  const response = await fetch(savedSkinFileUrl(skin), { cache: 'no-store' });
  if (!response.ok) {
    throw new Error(`Saved skin PNG download failed with HTTP ${response.status}.`);
  }
  const blob = await response.blob();
  if (blob.size < 1) {
    throw new Error('Saved skin PNG was empty.');
  }
  return blob;
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

export async function normalizeSkinUpload(file: File): Promise<SkinNormalizeMetadata> {
  const response = await fetch(apiUrl('/skins/normalize'), {
    method: 'POST',
    headers: { 'Content-Type': 'image/png' },
    body: file,
  });
  const payload = await response.json().catch(() => undefined);
  if (!response.ok) {
    throw apiResponseError(response, payload, `Skin validation failed with HTTP ${response.status}`);
  }
  const metadata = skinNormalizeMetadata(payload);
  if (!metadata) throw new Error('Skin validation returned an invalid response.');
  return metadata;
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
  const response = await fetch(apiUrl(`/skins/${textureKey}/texture?${params.toString()}`), {
    method: 'PUT',
    headers: { 'Content-Type': 'image/png' },
    body: file,
  });
  const payload = await response.json().catch(() => undefined);
  if (!response.ok) {
    throw apiResponseError(response, payload, `Texture replacement failed with HTTP ${response.status}`);
  }
  const saved = savedSkinRecord(payload);
  if (!saved) throw new Error('Texture replacement returned an invalid response.');
  return saved;
}

export async function resolveUploadSkinVariant(file: File, value: UploadSkinVariant): Promise<SkinVariant> {
  return value === 'auto' ? detectSkinVariantFromPng(file) : value;
}

export function uploadSkinName(file: File): string {
  return file.name.replace(/\.[^.]+$/, '').trim();
}

function minecraftProfile(value: unknown): MinecraftProfile | undefined {
  if (!isRecord(value)) return undefined;
  if (typeof value.id !== 'string' || typeof value.name !== 'string') return undefined;

  return {
    id: value.id,
    name: value.name,
    skins: Array.isArray(value.skins)
      ? value.skins.map(minecraftSkin).filter((skin): skin is MinecraftSkin => Boolean(skin))
      : [],
    capes: Array.isArray(value.capes)
      ? value.capes.map(minecraftCape).filter((cape): cape is MinecraftCape => Boolean(cape))
      : [],
  };
}

function minecraftSkinLookup(value: unknown): MinecraftSkinLookup | null {
  if (!isRecord(value)) return null;
  if (
    typeof value.username !== 'string' ||
    typeof value.uuid !== 'string' ||
    typeof value.source !== 'string' ||
    (value.variant !== 'classic' && value.variant !== 'slim') ||
    typeof value.texture_url !== 'string' ||
    typeof value.texture_file_url !== 'string' ||
    (value.cape_url !== null && value.cape_url !== undefined && typeof value.cape_url !== 'string') ||
    typeof value.head_url !== 'string'
  ) {
    return null;
  }

  return {
    username: value.username,
    uuid: value.uuid,
    source: value.source,
    variant: value.variant,
    texture_url: value.texture_url,
    texture_file_url: value.texture_file_url,
    cape_url: typeof value.cape_url === 'string' ? value.cape_url : null,
    head_url: value.head_url,
  };
}

export async function lookupMinecraftSkin(username: string): Promise<MinecraftSkinLookup> {
  const params = new URLSearchParams({ username, size: '96' });
  const payload = await api('GET', `/skin/lookup?${params.toString()}`);
  const parsed = minecraftSkinLookup(payload);
  if (!parsed) throw new Error('Minecraft skin lookup returned an invalid response.');
  return parsed;
}

export function minecraftReadiness(record: Record<string, unknown>): MinecraftAuthReadiness {
  return {
    minecraft_profile_ready:
      typeof record.minecraft_profile_ready === 'boolean' ? record.minecraft_profile_ready : undefined,
    minecraft_ownership_verified:
      typeof record.minecraft_ownership_verified === 'boolean' ? record.minecraft_ownership_verified : undefined,
    minecraft_profile: minecraftProfile(record.minecraft_profile),
    minecraft_token_expires_in:
      record.minecraft_token_expires_in === null ? null : maybeNumber(record.minecraft_token_expires_in),
  };
}

function accountActionState(value: unknown): AccountActionState | undefined {
  if (!isRecord(value)) return undefined;
  if (
    typeof value.state_id !== 'string' ||
    typeof value.label !== 'string' ||
    typeof value.enabled !== 'boolean' ||
    (value.disabled_reason !== undefined && typeof value.disabled_reason !== 'string') ||
    (value.detail !== undefined && typeof value.detail !== 'string') ||
    (value.success_summary !== undefined && typeof value.success_summary !== 'string')
  ) {
    return undefined;
  }
  return {
    state_id: value.state_id,
    label: value.label,
    enabled: value.enabled,
    disabled_reason: typeof value.disabled_reason === 'string' ? value.disabled_reason : undefined,
    detail: typeof value.detail === 'string' ? value.detail : undefined,
    success_summary: typeof value.success_summary === 'string' ? value.success_summary : undefined,
  };
}

function launcherAccount(value: unknown): LauncherAccount | null {
  if (
    !isRecord(value) ||
    typeof value.account_id !== 'string' ||
    (value.kind !== 'microsoft' && value.kind !== 'offline') ||
    typeof value.display_name !== 'string' ||
    typeof value.active !== 'boolean' ||
    typeof value.msa_authenticated !== 'boolean' ||
    typeof value.msa_refresh_available !== 'boolean'
  ) {
    return null;
  }

  return {
    account_id: value.account_id,
    kind: value.kind,
    display_name: value.display_name,
    active: value.active,
    login_id: typeof value.login_id === 'string' ? value.login_id : undefined,
    minecraft_profile_id: typeof value.minecraft_profile_id === 'string' ? value.minecraft_profile_id : undefined,
    offline_uuid: typeof value.offline_uuid === 'string' ? value.offline_uuid : undefined,
    msa_authenticated: value.msa_authenticated,
    msa_token_expires_in: value.msa_token_expires_in === null ? null : maybeNumber(value.msa_token_expires_in),
    msa_refresh_available: value.msa_refresh_available,
    online_action: accountActionState(value.online_action),
    refresh_action: accountActionState(value.refresh_action),
    profile_sync_action: accountActionState(value.profile_sync_action),
    view_model: isRecord(value.view_model)
      ? { detail: typeof value.view_model.detail === 'string' ? value.view_model.detail : undefined }
      : undefined,
    ...minecraftReadiness(value),
  };
}

export function launcherAccountsResponse(value: unknown): LauncherAccountsData | null {
  if (!isRecord(value) || !Array.isArray(value.accounts)) return null;
  if (value.active_account_id !== null && typeof value.active_account_id !== 'string') return null;
  return {
    active_account_id: value.active_account_id,
    accounts: value.accounts.map(launcherAccount).filter((account): account is LauncherAccount => account !== null),
  };
}

export function authStatusResponse(value: unknown): AuthStatusRecord | null {
  if (!isRecord(value)) return null;
  if (
    (value.launch_auth_mode !== 'offline' && value.launch_auth_mode !== 'online') ||
    typeof value.mode !== 'string' ||
    typeof value.username !== 'string' ||
    typeof value.uuid !== 'string' ||
    typeof value.provider !== 'string' ||
    typeof value.verified !== 'boolean' ||
    typeof value.skin_source !== 'string' ||
    typeof value.login_available !== 'boolean' ||
    typeof value.login_reason !== 'string' ||
    typeof value.msa_refresh_available !== 'boolean'
  ) {
    return null;
  }

  return {
    launch_auth_mode: value.launch_auth_mode,
    mode: value.mode,
    username: value.username,
    uuid: value.uuid,
    provider: value.provider,
    verified: value.verified,
    skin_source: value.skin_source,
    login_available: value.login_available,
    login_reason: value.login_reason,
    msa_authenticated: typeof value.msa_authenticated === 'boolean' ? value.msa_authenticated : undefined,
    msa_provider:
      typeof value.msa_provider === 'string' ? value.msa_provider : value.msa_provider === null ? null : undefined,
    msa_token_expires_in: value.msa_token_expires_in === null ? null : maybeNumber(value.msa_token_expires_in),
    msa_refresh_available: value.msa_refresh_available,
    online_action: accountActionState(value.online_action),
    refresh_action: accountActionState(value.refresh_action),
    profile_sync_action: accountActionState(value.profile_sync_action),
    skin_action: accountActionState(value.skin_action),
    ...minecraftReadiness(value),
  };
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
  if (isRecord(error)) {
    return boundedMessage(typeof error.error === 'string' ? error.error : undefined, fallback);
  }
  return boundedMessage(error instanceof Error ? error.message : undefined, fallback);
}

export function savedSkinApplyErrorMessage(error: unknown): string {
  return skinActionErrorMessage(error, 'Minecraft profile apply failed.');
}

export function savedSkinFileUrl(skin: SavedSkinRecord): string {
  return apiResourceUrl(`/skins/${skin.texture_key}/file`);
}

export async function defaultSkinFile(skin: DefaultSkin): Promise<File> {
  const response = await fetch(skin.src);
  const blob = await response.blob();
  return new File([blob], `${skin.name}.png`, { type: 'image/png' });
}

const defaultSkinKeyPromises = new Map<string, Promise<string>>();

export function defaultSkinTextureKey(skin: DefaultSkin): Promise<string> {
  const existing = defaultSkinKeyPromises.get(skin.id);
  if (existing) return existing;

  const pending = normalizeSkinUploadFromDefault(skin);
  defaultSkinKeyPromises.set(skin.id, pending);
  pending.catch(() => {
    if (defaultSkinKeyPromises.get(skin.id) === pending) defaultSkinKeyPromises.delete(skin.id);
  });
  return pending;
}

async function normalizeSkinUploadFromDefault(skin: DefaultSkin): Promise<string> {
  const metadata = await normalizeSkinUpload(await defaultSkinFile(skin));
  return metadata.textureKey;
}

let defaultSkinKeysPromise: Promise<Map<string, string>> | null = null;

export function defaultSkinTextureKeys(): Promise<Map<string, string>> {
  if (!defaultSkinKeysPromise) {
    const pending = (async () => {
      const entries = await Promise.all(
        DEFAULT_SKINS.map(async (skin) => {
          const textureKey = await defaultSkinTextureKey(skin);
          return [skin.id, textureKey] as const;
        }),
      );
      return new Map(entries);
    })();
    pending.catch(() => {
      if (defaultSkinKeysPromise === pending) defaultSkinKeysPromise = null;
    });
    defaultSkinKeysPromise = pending;
  }
  return defaultSkinKeysPromise;
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
