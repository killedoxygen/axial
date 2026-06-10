import type { JSX } from 'preact';
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { api, apiResourceUrl, apiUrl, isApiError } from '../../api';
import { Button, Card, IconButton, Input, Pill, SectionHeading, Segmented } from '../../ui/Atoms';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { showConfirm } from '../../ui/Dialog';
import { Icon } from '../../ui/Icons';
import { PlayerHeadPreview } from '../../ui/PlayerHeadPreview';
import { useTheme } from '../../hooks/use-theme';
import { clampPlayerNameInput, savePlayerName } from '../../player-name';
import { config } from '../../store';
import type { LaunchAuthMode } from '../../types';
import { validateUsername } from '../../utils';

interface SkinProfile {
  auth_mode: string;
  username: string;
  uuid: string;
  source: string;
  variant: string;
  texture_url: string | null;
  head_url: string | null;
}

interface MinecraftSkin {
  id: string;
  state: string;
  url: string;
  variant: string;
}

interface MinecraftCape {
  id: string;
  state: string;
  url: string;
}

interface MinecraftProfile {
  id: string;
  name: string;
  skins: MinecraftSkin[];
  capes: MinecraftCape[];
}

interface MinecraftAuthReadiness {
  minecraft_profile_ready?: boolean;
  minecraft_ownership_verified?: boolean;
  minecraft_profile?: MinecraftProfile;
  minecraft_token_expires_in?: number | null;
}

interface AuthStatus {
  launch_auth_mode: LaunchAuthMode;
  mode: string;
  username: string;
  uuid: string;
  provider: string;
  verified: boolean;
  online_mode_ready: boolean;
  skin_source: string;
  login_available: boolean;
  login_reason: string;
  msa_authenticated?: boolean;
  msa_provider?: string | null;
  msa_token_expires_in?: number | null;
  msa_refresh_available: boolean;
}

type AuthStatusRecord = AuthStatus & MinecraftAuthReadiness;

interface AuthLoginPending {
  status: 'pending';
  login_id: string;
  user_code: string;
  verification_uri: string;
  expires_in: number;
  interval: number;
  message?: string;
}

interface AuthPollPending {
  status: 'pending';
  interval: number;
  poll_hint?: string;
}

interface AuthPollAuthenticated {
  status: 'msa_authenticated';
  msa_provider?: string | null;
  msa_token_expires_in?: number | null;
}

type AuthPollAuthenticatedRecord = AuthPollAuthenticated & MinecraftAuthReadiness;

type AuthPollTerminalStatus =
  | 'authorization_declined'
  | 'expired'
  | 'bad_verification_code'
  | 'minecraft_auth_chain_failed'
  | 'error';

interface AuthPollTerminal {
  status: AuthPollTerminalStatus;
  error?: string;
  auth_chain_error?: string;
  poll_hint?: string;
}

type AuthPollResponse = AuthPollPending | AuthPollAuthenticatedRecord | AuthPollTerminal;

type OfflineProfileState = 'loading' | 'ready' | 'unavailable';
type AuthStatusState = 'loading' | 'ready' | 'unavailable';
type CopyTarget = 'code' | 'url';
type SkinVariant = 'classic' | 'slim';
type UploadSkinVariant = SkinVariant | 'auto';
type SavedSkinPreviewSide = 'front' | 'back';
type SavedSkinLayerMode = 'full' | 'base';
type SavedSkinSort = 'recent' | 'name' | 'equipped' | 'source';

interface SavedSkinRecord {
  texture_key: string;
  name: string;
  variant: SkinVariant;
  source: string;
  created_at: string;
  updated_at: string;
  applied_at: string | null;
  byte_size: number;
}

interface StagedSkinUpload {
  file: File;
  objectUrl: string;
  detectedVariant: SkinVariant;
  detectingVariant: boolean;
  normalizeStatus: 'checking' | 'ready' | 'error';
  normalizeError?: string;
  textureKey?: string;
  originalWidth?: number;
  originalHeight?: number;
  normalizedByteSize?: number;
  applyAfterSave: boolean;
}

interface SkinNormalizeMetadata {
  textureKey: string;
  variantSuggestion: SkinVariant;
  originalWidth: number;
  originalHeight: number;
  normalizedByteSize: number;
}

function PlayerIdentityEditor({
  savedUsername,
  headSrc,
  textureSrc,
  profileUsername,
  profileSource,
}: {
  savedUsername: string;
  headSrc?: string;
  textureSrc?: string;
  profileUsername?: string;
  profileSource?: string;
}): JSX.Element {
  const theme = useTheme();
  const [username, setUsername] = useState(savedUsername);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const nameError = validateUsername(username);
  const nameValid = nameError === null;
  const showNameError = username.length > 0 && !nameValid;
  const previewSrc = username.trim() === savedUsername ? headSrc : undefined;
  const previewTextureSrc = username.trim() === savedUsername ? textureSrc : undefined;
  const previewProfileName = profileUsername || username.trim() || 'Player';
  const previewOnline = profileSource === 'minecraft_profile_skin' && Boolean(previewTextureSrc);
  const playerNameLabel = previewOnline ? 'Offline player name' : 'Player name';

  const save = async (): Promise<void> => {
    const next = username.trim();
    if (!nameValid || next === savedUsername) return;
    setSaving(true);
    setSaveError(null);
    try {
      const saved = await savePlayerName(next);
      if (!saved) return;
    } catch {
      setSaveError('Could not save player name. Try again.');
    } finally {
      setSaving(false);
    }
  };

  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 18, flexWrap: 'wrap' }}>
      <PlayerHeadPreview
        username={username}
        src={previewSrc}
        textureSrc={previewTextureSrc}
        size={96}
        radius={theme.r.md}
        ariaLabel={`${previewOnline ? 'Minecraft profile skin' : 'Offline skin preview'} for ${previewProfileName}`}
        title={previewOnline ? 'Minecraft profile skin' : 'Offline skin preview'}
      />
      <div style={{ flex: 1, minWidth: 240 }}>
        <div style={{
          fontSize: 11, fontWeight: 600, color: theme.n.textMute,
          textTransform: 'uppercase', letterSpacing: 0, marginBottom: 6,
        }}>{playerNameLabel}</div>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
          <Input
            value={username}
            onChange={(v) => {
              setUsername(clampPlayerNameInput(v));
              setSaveError(null);
            }}
            placeholder="Player"
            style={{ maxWidth: 360 }}
          />
          <Button onClick={save} disabled={saving || !nameValid || username.trim() === savedUsername} sound="affirm">
            Save
          </Button>
          {showNameError && (
            <span style={{ fontSize: 12, fontWeight: 500, color: 'var(--err)' }}>
              {nameError}
            </span>
          )}
          {saveError && (
            <span style={{ fontSize: 12, fontWeight: 500, color: 'var(--err)' }}>
              {saveError}
            </span>
          )}
        </div>
        {previewOnline && (
          <div style={{
            marginTop: 7,
            color: theme.n.textMute,
            fontSize: 12,
            fontWeight: 500,
            lineHeight: 1.35,
            overflowWrap: 'anywhere',
          }}>
            Showing {previewProfileName} skin from Minecraft profile.
          </div>
        )}
      </div>
    </div>
  );
}

function shortenUuid(uuid: string): string {
  return uuid.length > 24 ? `${uuid.slice(0, 8)}...${uuid.slice(-12)}` : uuid;
}

function ProfileMetaValue({ label, value }: { label: string; value: string }): JSX.Element {
  const theme = useTheme();

  return (
    <div style={{ display: 'grid', gap: 3, minWidth: 0 }}>
      <div style={{
        fontSize: 10,
        fontWeight: 700,
        color: theme.n.textMute,
        textTransform: 'uppercase',
        letterSpacing: 0,
      }}>{label}</div>
      <div style={{
        color: theme.n.textDim,
        fontFamily: label === 'UUID' ? theme.font.sans : undefined,
        fontSize: 12,
        lineHeight: 1.35,
        minWidth: 0,
        overflowWrap: 'anywhere',
        wordBreak: 'break-word',
      }}>{value}</div>
    </div>
  );
}

function useOfflineSkinProfile(savedUsername: string): {
  profile: SkinProfile | null;
  state: OfflineProfileState;
} {
  const [profile, setProfile] = useState<SkinProfile | null>(null);
  const [state, setState] = useState<OfflineProfileState>('loading');

  useEffect(() => {
    let active = true;
    setState('loading');
    setProfile(null);

    void api('GET', '/skin/profile')
      .then((res: SkinProfile & { error?: string }) => {
        if (!active) return;
        if (res.error) throw new Error(res.error);
        setProfile(res);
        setState('ready');
      })
      .catch(() => {
        if (!active) return;
        setProfile(null);
        setState('unavailable');
      });

    return () => {
      active = false;
    };
  }, [savedUsername]);

  return { profile, state };
}

function useAuthStatus(savedUsername: string): {
  status: AuthStatusRecord | null;
  state: AuthStatusState;
  refresh: () => void;
} {
  const [status, setStatus] = useState<AuthStatusRecord | null>(null);
  const [state, setState] = useState<AuthStatusState>('loading');
  const [refreshIndex, setRefreshIndex] = useState(0);

  const refresh = useCallback(() => {
    setRefreshIndex((value) => value + 1);
  }, []);

  useEffect(() => {
    let active = true;
    setState('loading');
    setStatus(null);

    void api('GET', '/auth/status')
      .then((res: unknown) => {
        if (!active) return;
        if (isRecord(res) && typeof res.error === 'string') throw new Error(res.error);
        const parsed = authStatusResponse(res);
        if (!parsed) throw new Error('invalid auth status');
        setStatus(parsed);
        setState('ready');
      })
      .catch(() => {
        if (!active) return;
        setStatus(null);
        setState('unavailable');
      });

    return () => {
      active = false;
    };
  }, [savedUsername, refreshIndex]);

  return { status, state, refresh };
}

function useSavedSkins(): {
  skins: SavedSkinRecord[];
  state: AuthStatusState;
  error: string | null;
  refresh: () => void;
} {
  const [skins, setSkins] = useState<SavedSkinRecord[]>([]);
  const [state, setState] = useState<AuthStatusState>('loading');
  const [error, setError] = useState<string | null>(null);
  const [refreshIndex, setRefreshIndex] = useState(0);

  const refresh = useCallback(() => {
    setRefreshIndex((value) => value + 1);
  }, []);

  useEffect(() => {
    let active = true;
    setState('loading');
    setError(null);

    void api('GET', '/skins')
      .then((res: unknown) => {
        if (!active) return;
        const parsed = savedSkinsResponse(res);
        if (!parsed) throw new Error('invalid saved skins response');
        setSkins(parsed);
        setState('ready');
      })
      .catch((err: unknown) => {
        if (!active) return;
        setSkins([]);
        setState('unavailable');
        setError(err instanceof Error ? err.message : 'Saved skins are unavailable.');
      });

    return () => {
      active = false;
    };
  }, [refreshIndex]);

  return { skins, state, error, refresh };
}

function skinSourceLabel(source: string, authMode: string): string {
  if (source === 'minecraft_profile_skin') return 'Minecraft profile skin';
  if (authMode === 'online') return 'Default skin';
  return 'Offline default';
}

function savedSkinSourceLabel(source: string): string {
  switch (source) {
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

function sortSavedSkins(skins: SavedSkinRecord[], sort: SavedSkinSort): SavedSkinRecord[] {
  return skins
    .map((skin, index) => ({ skin, index }))
    .sort((left, right) => {
      const nameCompare = savedSkinCompareByName(left.skin, right.skin);
      const recentCompare = savedSkinRecentTime(right.skin) - savedSkinRecentTime(left.skin);

      let result = 0;
      if (sort === 'name') {
        result = nameCompare || recentCompare;
      } else if (sort === 'equipped') {
        result = Number(Boolean(right.skin.applied_at)) - Number(Boolean(left.skin.applied_at))
          || recentCompare
          || nameCompare;
      } else if (sort === 'source') {
        result = savedSkinSourceLabel(left.skin.source).localeCompare(
          savedSkinSourceLabel(right.skin.source),
          undefined,
          { numeric: true, sensitivity: 'base' },
        ) || nameCompare || recentCompare;
      } else {
        result = recentCompare || nameCompare;
      }

      return result || left.index - right.index;
    })
    .map(({ skin }) => skin);
}

function savedSkinMatchesFilter(skin: SavedSkinRecord, filter: string): boolean {
  if (!filter) return true;
  return skin.name.toLowerCase().includes(filter) ||
    savedSkinSourceLabel(skin.source).toLowerCase().includes(filter) ||
    skin.variant.toLowerCase().includes(filter) ||
    skin.texture_key.toLowerCase().startsWith(filter);
}

function SkinProfileMeta({
  profile,
  state,
}: {
  profile: SkinProfile | null;
  state: OfflineProfileState;
}): JSX.Element {
  const theme = useTheme();

  return (
    <div style={{
      marginTop: 14,
      paddingTop: 12,
      borderTop: '1px solid var(--line)',
      display: 'flex',
      alignItems: 'center',
      gap: 14,
      flexWrap: 'wrap',
    }}>
      <div style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 6,
        color: theme.n.textMute,
        fontSize: 12,
        fontWeight: 600,
      }}>
        <Icon name="tag" size={14} color={theme.n.textMute} />
        Skin profile
      </div>

      {state === 'ready' && profile ? (
        <div style={{
          display: 'flex',
          flexWrap: 'wrap',
          gap: 14,
          minWidth: 0,
        }}>
          <ProfileMetaValue label="UUID" value={shortenUuid(profile.uuid)} />
          <ProfileMetaValue label="Variant" value={profile.variant || 'classic'} />
          <ProfileMetaValue label="Source" value={skinSourceLabel(profile.source, profile.auth_mode)} />
        </div>
      ) : (
        <div style={{
          color: theme.n.textMute,
          fontSize: 12,
          fontWeight: 500,
        }}>
          {state === 'loading' ? 'Loading profile...' : 'Profile unavailable'}
        </div>
      )}
    </div>
  );
}

function PlayerIdentityCard({ savedUsername }: { savedUsername: string }): JSX.Element {
  const { profile, state } = useOfflineSkinProfile(savedUsername);
  const headSrc = state === 'ready' && profile?.head_url
    ? apiResourceUrl(profile.head_url)
    : undefined;
  const textureSrc = state === 'ready'
    && profile?.source === 'minecraft_profile_skin'
    && profile.texture_url
    ? apiResourceUrl('/skin/profile/file')
    : undefined;

  return (
    <Card>
      <SectionHeading title="Identity" />
      <PlayerIdentityEditor
        key={savedUsername}
        savedUsername={savedUsername}
        headSrc={headSrc}
        textureSrc={textureSrc}
        profileUsername={profile?.username}
        profileSource={profile?.source}
      />
      <SkinProfileMeta profile={profile} state={state} />
    </Card>
  );
}

async function copyText(text: string): Promise<void> {
  if (!navigator.clipboard) {
    throw new Error('clipboard API unavailable');
  }
  await navigator.clipboard.writeText(text);
}

function formatSeconds(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return 'unknown';
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const minutes = Math.floor(seconds / 60);
  const remaining = Math.round(seconds % 60);
  return remaining > 0 ? `${minutes}m ${remaining}s` : `${minutes}m`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function maybeNumber(value: unknown): number | undefined {
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
  if (
    typeof value.id !== 'string' ||
    typeof value.state !== 'string' ||
    typeof value.url !== 'string'
  ) {
    return null;
  }

  return {
    id: value.id,
    state: value.state,
    url: value.url,
  };
}

function savedSkinRecord(value: unknown): SavedSkinRecord | null {
  if (!isRecord(value)) return null;
  if (
    typeof value.texture_key !== 'string' ||
    typeof value.name !== 'string' ||
    (value.variant !== 'classic' && value.variant !== 'slim') ||
    typeof value.source !== 'string' ||
    typeof value.created_at !== 'string' ||
    typeof value.updated_at !== 'string' ||
    (
      value.applied_at !== undefined &&
      value.applied_at !== null &&
      typeof value.applied_at !== 'string'
    ) ||
    typeof value.byte_size !== 'number'
  ) {
    return null;
  }

  return {
    texture_key: value.texture_key,
    name: value.name,
    variant: value.variant,
    source: value.source,
    created_at: value.created_at,
    updated_at: value.updated_at,
    applied_at: typeof value.applied_at === 'string' ? value.applied_at : null,
    byte_size: value.byte_size,
  };
}

function savedSkinsResponse(value: unknown): SavedSkinRecord[] | null {
  if (!isRecord(value) || !Array.isArray(value.skins)) return null;
  return value.skins.map(savedSkinRecord).filter((skin): skin is SavedSkinRecord => Boolean(skin));
}

function skinNormalizeMetadata(value: unknown): SkinNormalizeMetadata | null {
  if (!isRecord(value)) return null;
  if (
    typeof value.texture_key !== 'string' ||
    (value.variant_suggestion !== 'classic' && value.variant_suggestion !== 'slim') ||
    typeof value.original_width !== 'number' ||
    typeof value.original_height !== 'number' ||
    typeof value.normalized_byte_size !== 'number'
  ) {
    return null;
  }

  return {
    textureKey: value.texture_key,
    variantSuggestion: value.variant_suggestion,
    originalWidth: value.original_width,
    originalHeight: value.original_height,
    normalizedByteSize: value.normalized_byte_size,
  };
}

function skinVariantValue(value: string | undefined): SkinVariant {
  return value?.toLowerCase() === 'slim' ? 'slim' : 'classic';
}

function stagedSkinVariant(staged: StagedSkinUpload, selectedVariant: UploadSkinVariant): SkinVariant {
  return selectedVariant === 'auto' ? staged.detectedVariant : selectedVariant;
}

function activeMinecraftSkin(profile: MinecraftProfile | undefined): MinecraftSkin | null {
  if (!profile) return null;
  return profile.skins.find((skin) => skin.state.toLowerCase() === 'active')
    ?? profile.skins[0]
    ?? null;
}

function isPngFile(file: File): boolean {
  const type = file.type.trim().toLowerCase();
  if (type) return type === 'image/png';
  return file.name.toLowerCase().endsWith('.png');
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

async function detectSkinVariantFromSavedSkin(skin: SavedSkinRecord): Promise<SkinVariant> {
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

async function normalizeSkinUpload(file: File): Promise<SkinNormalizeMetadata> {
  const response = await fetch(apiUrl('/skins/normalize'), {
    method: 'POST',
    headers: { 'Content-Type': 'image/png' },
    body: file,
  });
  const payload = await response.json().catch(() => undefined);
  if (!response.ok) {
    throw new Error(readApiPayloadMessage(payload, `Skin validation failed with HTTP ${response.status}`));
  }
  const metadata = skinNormalizeMetadata(payload);
  if (!metadata) throw new Error('Skin validation returned an invalid response.');
  return metadata;
}

async function resolveUploadSkinVariant(file: File, value: UploadSkinVariant): Promise<SkinVariant> {
  return value === 'auto' ? detectSkinVariantFromPng(file) : value;
}

function uploadSkinName(file: File): string {
  return file.name.replace(/\.[^.]+$/, '').trim();
}

function minecraftProfile(value: unknown): MinecraftProfile | undefined {
  if (!isRecord(value)) return undefined;
  if (typeof value.id !== 'string' || typeof value.name !== 'string') return undefined;

  return {
    id: value.id,
    name: value.name,
    skins: Array.isArray(value.skins) ? value.skins.map(minecraftSkin).filter((skin): skin is MinecraftSkin => Boolean(skin)) : [],
    capes: Array.isArray(value.capes) ? value.capes.map(minecraftCape).filter((cape): cape is MinecraftCape => Boolean(cape)) : [],
  };
}

function minecraftReadiness(record: Record<string, unknown>): MinecraftAuthReadiness {
  return {
    minecraft_profile_ready: typeof record.minecraft_profile_ready === 'boolean'
      ? record.minecraft_profile_ready
      : undefined,
    minecraft_ownership_verified: typeof record.minecraft_ownership_verified === 'boolean'
      ? record.minecraft_ownership_verified
      : undefined,
    minecraft_profile: minecraftProfile(record.minecraft_profile),
    minecraft_token_expires_in: record.minecraft_token_expires_in === null
      ? null
      : maybeNumber(record.minecraft_token_expires_in),
  };
}

function authStatusResponse(value: unknown): AuthStatusRecord | null {
  if (!isRecord(value)) return null;
  if (
    (value.launch_auth_mode !== 'offline' && value.launch_auth_mode !== 'online') ||
    typeof value.mode !== 'string' ||
    typeof value.username !== 'string' ||
    typeof value.uuid !== 'string' ||
    typeof value.provider !== 'string' ||
    typeof value.verified !== 'boolean' ||
    typeof value.online_mode_ready !== 'boolean' ||
    typeof value.skin_source !== 'string' ||
    typeof value.login_available !== 'boolean' ||
    typeof value.login_reason !== 'string'
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
    online_mode_ready: value.online_mode_ready,
    skin_source: value.skin_source,
    login_available: value.login_available,
    login_reason: value.login_reason,
    msa_authenticated: typeof value.msa_authenticated === 'boolean' ? value.msa_authenticated : undefined,
    msa_provider: typeof value.msa_provider === 'string' ? value.msa_provider : value.msa_provider === null ? null : undefined,
    msa_token_expires_in: value.msa_token_expires_in === null ? null : maybeNumber(value.msa_token_expires_in),
    msa_refresh_available: value.msa_refresh_available === true,
    ...minecraftReadiness(value),
  };
}

function loginPendingResponse(value: unknown): AuthLoginPending | null {
  if (!isRecord(value)) return null;
  const record = value;
  if (
    record.status !== 'pending' ||
    typeof record.login_id !== 'string' ||
    typeof record.user_code !== 'string' ||
    typeof record.verification_uri !== 'string' ||
    typeof record.expires_in !== 'number' ||
    typeof record.interval !== 'number'
  ) {
    return null;
  }

  return {
    status: 'pending',
    login_id: record.login_id,
    user_code: record.user_code,
    verification_uri: record.verification_uri,
    expires_in: record.expires_in,
    interval: record.interval,
    message: typeof record.message === 'string' ? record.message : undefined,
  };
}

function boundedMessage(value: string | undefined, fallback: string): string {
  const trimmed = value?.trim();
  if (!trimmed) return fallback;
  return trimmed.length > 180 ? `${trimmed.slice(0, 177)}...` : trimmed;
}

function apiErrorMessage(value: unknown, fallback: string): string {
  if (!isRecord(value)) {
    return fallback;
  }
  return boundedMessage(typeof value.error === 'string' ? value.error : undefined, fallback);
}

function loginErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not start Microsoft sign-in.');
}

function logoutErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not clear Microsoft sign-in.');
}

function authRefreshErrorMessage(value: unknown): string {
  if (isRecord(value)) {
    if (value.status === 'minecraft_auth_chain_failed') {
      return 'Croopor could not verify the Minecraft profile or ownership during refresh. Online is not ready. Offline remains available. Re-verify with Microsoft if Online is needed.';
    }
    if (value.status === 'sign_in_required') {
      return 'Microsoft sign-in needs re-verification before Online can be used. Offline remains available.';
    }
    if (value.status === 'refresh_failed') {
      return 'Microsoft sign-in could not be refreshed. Offline remains available. Re-verify with Microsoft if Online is needed.';
    }
  }
  return apiErrorMessage(value, 'Could not refresh Microsoft sign-in. Re-verify with Microsoft or use Offline.');
}

function configErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not save launch mode.');
}

function pollResponse(value: unknown): AuthPollResponse | null {
  if (!isRecord(value)) return null;
  const record = value;
  if (record.status === 'pending' && typeof record.interval === 'number') {
    return {
      status: 'pending',
      interval: record.interval,
      poll_hint: typeof record.poll_hint === 'string' ? record.poll_hint : undefined,
    };
  }

  if (record.status === 'msa_authenticated') {
    return {
      status: 'msa_authenticated',
      msa_provider: typeof record.msa_provider === 'string' ? record.msa_provider : undefined,
      msa_token_expires_in: record.msa_token_expires_in === null ? null : maybeNumber(record.msa_token_expires_in),
      ...minecraftReadiness(record),
    };
  }

  if (
    record.status === 'authorization_declined' ||
    record.status === 'expired' ||
    record.status === 'bad_verification_code' ||
    record.status === 'minecraft_auth_chain_failed' ||
    record.status === 'error'
  ) {
    return {
      status: record.status,
      error: typeof record.error === 'string' ? record.error : undefined,
      auth_chain_error: typeof record.auth_chain_error === 'string' ? record.auth_chain_error : undefined,
      poll_hint: typeof record.poll_hint === 'string' ? record.poll_hint : undefined,
    };
  }

  return null;
}

function pollTerminalMessage(response: AuthPollTerminal | null): string {
  if (!response) return 'Microsoft sign-in returned an unexpected response.';
  if (response.status === 'minecraft_auth_chain_failed') {
    return 'Microsoft sign-in completed, but Croopor could not verify the Minecraft profile or ownership. Offline launch mode remains available.';
  }
  const fallback = response.status === 'authorization_declined'
    ? 'Microsoft sign-in was declined.'
    : response.status === 'expired'
      ? 'Microsoft sign-in expired. Get a new code to try again.'
      : response.status === 'bad_verification_code'
        ? 'Microsoft sign-in code was rejected. Get a new code to try again.'
        : 'Microsoft sign-in could not be completed.';
  return boundedMessage(response.error || response.poll_hint, fallback);
}

function pollErrorMessage(value: unknown): string {
  const response = pollResponse(value);
  if (response && response.status !== 'pending' && response.status !== 'msa_authenticated') {
    return pollTerminalMessage(response);
  }
  return apiErrorMessage(value, 'Microsoft sign-in could not be completed.');
}

function hasMinecraftReadiness(status: MinecraftAuthReadiness): boolean {
  return typeof status.minecraft_profile_ready === 'boolean' ||
    typeof status.minecraft_ownership_verified === 'boolean' ||
    Boolean(status.minecraft_profile) ||
    typeof status.minecraft_token_expires_in === 'number';
}

function readinessValue(value: boolean | undefined, readyLabel: string, notReadyLabel: string): string {
  if (value === true) return readyLabel;
  if (value === false) return notReadyLabel;
  return 'Not reported';
}

function expiryWindowValue(seconds: number | null | undefined): string {
  if (typeof seconds !== 'number' || !Number.isFinite(seconds)) return 'Not reported';
  if (seconds <= 0) return 'Expired';
  return `Expires in ${formatSeconds(seconds)}`;
}

function compactExpiryWindow(label: string, seconds: number | null | undefined): string {
  if (typeof seconds !== 'number' || !Number.isFinite(seconds)) return `${label} not reported`;
  if (seconds <= 0) return `${label} expired`;
  return `${label} ${formatSeconds(seconds)}`;
}

function expiryWindowTone(seconds: number | null | undefined): 'info' | 'warn' {
  return typeof seconds === 'number' && Number.isFinite(seconds) && seconds > 300 ? 'info' : 'warn';
}

function launchAuthMode(value: unknown): LaunchAuthMode {
  return value === 'online' ? 'online' : 'offline';
}

function statusCanSelectOnline(status: AuthStatusRecord): boolean {
  if (status.online_mode_ready) return true;
  return status.minecraft_profile_ready === true &&
    status.minecraft_ownership_verified === true &&
    typeof status.minecraft_token_expires_in === 'number' &&
    status.minecraft_token_expires_in > 0;
}

function LaunchAuthModeOption({
  active,
  disabled,
  icon,
  title,
  description,
  onClick,
}: {
  active: boolean;
  disabled?: boolean;
  icon: string;
  title: string;
  description: string;
  onClick: () => void;
}): JSX.Element {
  const theme = useTheme();

  return (
    <button
      type="button"
      onClick={disabled ? undefined : onClick}
      disabled={disabled}
      aria-pressed={active}
      style={{
        display: 'grid',
        gridTemplateColumns: 'auto 1fr',
        gap: 10,
        alignItems: 'start',
        minWidth: 0,
        padding: '12px 13px',
        border: `1px solid ${active ? 'var(--accent)' : 'var(--line)'}`,
        borderRadius: theme.r.md,
        background: active ? theme.n.surface : theme.n.surface2,
        color: disabled ? theme.n.textMute : theme.n.text,
        cursor: disabled ? 'not-allowed' : 'pointer',
        opacity: disabled ? 0.62 : 1,
        textAlign: 'left',
      }}
      title={disabled ? `${title} is unavailable until a verified Minecraft account is ready.` : title}
    >
      <Icon name={active ? 'check-circle' : icon} size={16} color={active ? 'var(--accent)' : theme.n.textMute} />
      <span style={{ display: 'grid', gap: 4, minWidth: 0 }}>
        <span style={{
          fontSize: 13,
          fontWeight: 750,
          lineHeight: 1.2,
        }}>{title}</span>
        <span style={{
          color: disabled ? theme.n.textMute : theme.n.textDim,
          fontSize: 12,
          lineHeight: 1.4,
        }}>{description}</span>
      </span>
    </button>
  );
}

function launchAuthModeCopy(
  mode: LaunchAuthMode,
  status: AuthStatusRecord,
  onlineSelectable: boolean,
): { tone: 'info' | 'ok' | 'warn'; icon: string; text: string } {
  if (mode === 'offline') {
    return {
      tone: onlineSelectable ? 'info' : 'ok',
      icon: 'shield-check',
      text: onlineSelectable
        ? 'Offline is selected. It is the reliable default; Online is available while the verified account credentials remain valid.'
        : 'Offline is selected. It stays available for singleplayer and offline-mode servers even when Microsoft sign-in is unavailable.',
    };
  }

  if (status.online_mode_ready) {
    return {
      tone: 'ok',
      icon: 'check-circle',
      text: 'Online is selected and the backend reports launch credentials ready. Croopor can refresh online credentials when secure Microsoft auth is available.',
    };
  }

  return {
    tone: 'warn',
    icon: 'alert',
    text: status.login_available
      ? 'Online is selected, but launch credentials are missing or expired. Refresh if available, re-verify with Microsoft, or switch to Offline.'
      : 'Online is selected, but launch credentials are missing or expired. Sign-in is unavailable right now; switch to Offline.',
  };
}

function AuthModeControl({
  status,
  onSaved,
}: {
  status: AuthStatusRecord;
  onSaved: () => void;
}): JSX.Element {
  const theme = useTheme();
  const savedMode = launchAuthMode(config.value?.launch_auth_mode ?? status.launch_auth_mode);
  const onlineSelectable = statusCanSelectOnline(status);
  const [savingMode, setSavingMode] = useState<LaunchAuthMode | null>(null);
  const [message, setMessage] = useState<{ tone: 'ok' | 'err'; text: string } | null>(null);
  const modeCopy = launchAuthModeCopy(savedMode, status, onlineSelectable);

  const saveMode = async (nextMode: LaunchAuthMode): Promise<void> => {
    if (nextMode === savedMode || savingMode) return;
    if (nextMode === 'online' && !onlineSelectable) {
      setMessage({
        tone: 'err',
        text: 'Online cannot be selected until a non-expired, Java-owning Minecraft account is verified.',
      });
      return;
    }

    setSavingMode(nextMode);
    setMessage(null);
    try {
      const response = await api('PUT', '/config', { launch_auth_mode: nextMode });
      if (isRecord(response) && typeof response.error === 'string') {
        setMessage({ tone: 'err', text: configErrorMessage(response) });
        return;
      }
      config.value = response;
      setMessage({
        tone: 'ok',
        text: nextMode === 'online'
          ? 'Online launch mode saved. Croopor can refresh online credentials when secure Microsoft auth is available.'
          : 'Offline launch mode saved. Offline remains the reliable default.',
      });
      onSaved();
    } catch {
      setMessage({ tone: 'err', text: 'Could not reach the local backend to save launch mode.' });
    } finally {
      setSavingMode(null);
    }
  };

  return (
    <div style={{
      display: 'grid',
      gap: 10,
      padding: '12px 14px',
      border: '1px solid var(--line)',
      borderRadius: theme.r.md,
      background: theme.n.surface2,
    }}>
      <div style={{
        display: 'flex',
        justifyContent: 'space-between',
        alignItems: 'center',
        gap: 12,
        flexWrap: 'wrap',
      }}>
        <div style={{ display: 'grid', gap: 3 }}>
          <div style={{
            fontSize: 11,
            fontWeight: 700,
            color: theme.n.textMute,
            textTransform: 'uppercase',
            letterSpacing: 0,
          }}>Launch auth mode</div>
          <div style={{ color: theme.n.textDim, fontSize: 12, lineHeight: 1.4 }}>
            Choose the identity Croopor should use when launching Minecraft.
          </div>
        </div>
        <Pill tone={modeCopy.tone} icon={modeCopy.icon}>
          {savedMode === 'online' ? 'Online selected' : 'Offline selected'}
        </Pill>
      </div>

      <div style={{
        display: 'grid',
        gridTemplateColumns: 'repeat(2, minmax(220px, 1fr))',
        gap: 10,
      }}>
        <LaunchAuthModeOption
          active={savedMode === 'offline'}
          icon="shield-check"
          title={savingMode === 'offline' ? 'Saving Offline' : 'Offline'}
          description="Reliable default. Uses the offline profile and does not need Microsoft credentials."
          onClick={() => void saveMode('offline')}
          disabled={savingMode !== null}
        />
        <LaunchAuthModeOption
          active={savedMode === 'online'}
          icon="globe"
          title={savingMode === 'online' ? 'Saving Online' : 'Online'}
          description={onlineSelectable
            ? 'Uses the verified Minecraft profile while these credentials are valid.'
            : 'Unavailable until sign-in verifies a Java-owning Minecraft profile.'}
          onClick={() => void saveMode('online')}
          disabled={savingMode !== null || (!onlineSelectable && savedMode !== 'online')}
        />
      </div>

      <div style={{
        display: 'flex',
        alignItems: 'flex-start',
        gap: 8,
        color: theme.n.textDim,
        fontSize: 12,
        lineHeight: 1.45,
      }}>
        <Icon name={modeCopy.icon} size={15} color={modeCopy.tone === 'warn' ? 'var(--warn)' : theme.n.textMute} style={{ marginTop: 1 }} />
        <span>{modeCopy.text}</span>
      </div>

      {message && (
        <div style={{
          color: message.tone === 'err' ? 'var(--err)' : theme.n.textDim,
          fontSize: 12,
          fontWeight: 500,
          lineHeight: 1.4,
        }}>
          {message.text}
        </div>
      )}
    </div>
  );
}

function pollSuccessMessage(poll: AuthPollAuthenticatedRecord): string {
  const profileName = poll.minecraft_profile?.name;
  if (poll.minecraft_profile_ready && poll.minecraft_ownership_verified) {
    return `${profileName || 'Minecraft profile'} was verified. Online launch mode can be selected while these credentials remain valid. Croopor can refresh them when secure Microsoft auth is available.`;
  }
  if (poll.minecraft_profile_ready) {
    return `${profileName || 'Minecraft profile'} was found, but ownership was not verified. Offline launch mode remains available.`;
  }
  return 'Microsoft sign-in is active, but Minecraft profile verification is not complete. Offline launch mode remains available.';
}

function authRefreshSuccessMessage(value: unknown): string {
  const readiness = isRecord(value) ? minecraftReadiness(value) : {};
  const profileName = readiness.minecraft_profile?.name;
  if (readiness.minecraft_profile_ready && readiness.minecraft_ownership_verified) {
    return `${profileName || 'Minecraft profile'} was refreshed. Online launch mode can use it while the refreshed credentials remain valid.`;
  }
  return 'Microsoft sign-in refresh completed, but Minecraft profile readiness is still not complete. Re-verify or use Offline if Online remains unavailable.';
}

function MinecraftProfileReadiness({ status }: { status: AuthStatusRecord }): JSX.Element | null {
  const theme = useTheme();
  if (!hasMinecraftReadiness(status)) return null;

  const profile = status.minecraft_profile;
  const profileReady = status.minecraft_profile_ready === true;
  const ownershipVerified = status.minecraft_ownership_verified === true;
  const primarySkin = profile?.skins[0];
  const skinSummary = profile
    ? `${profile.skins.length}${primarySkin ? `, ${primarySkin.variant || 'classic'} ${primarySkin.state}` : ''}`
    : 'Not reported';
  const verificationWindow = typeof status.minecraft_token_expires_in === 'number'
    ? expiryWindowValue(status.minecraft_token_expires_in)
    : 'Not reported';

  return (
    <div style={{
      display: 'grid',
      gap: 10,
      padding: '12px 14px',
      border: '1px solid var(--line)',
      borderRadius: theme.r.md,
      background: theme.n.surface2,
    }}>
      <div style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        gap: 10,
        flexWrap: 'wrap',
      }}>
        <div style={{
          display: 'inline-flex',
          alignItems: 'center',
          gap: 7,
          color: theme.n.textDim,
          fontSize: 12,
          fontWeight: 700,
        }}>
          <Icon name="shield-check" size={15} color={theme.n.textMute} />
          Minecraft profile readiness
        </div>
        <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
          <Pill tone={profileReady ? 'ok' : 'warn'} icon={profileReady ? 'check-circle' : 'alert'}>
            {profileReady ? 'Profile verified' : 'Profile not verified'}
          </Pill>
          <Pill tone={ownershipVerified ? 'ok' : 'warn'} icon={ownershipVerified ? 'check-circle' : 'alert'}>
            {ownershipVerified ? 'Ownership verified' : 'Ownership not verified'}
          </Pill>
        </div>
      </div>
      <div style={{
        color: theme.n.textDim,
        fontSize: 12,
        lineHeight: 1.45,
      }}>
        Online launch mode can use this profile only while Online is selected and these credentials are inside the reported expiry window. Croopor can refresh them when secure Microsoft auth is available; otherwise re-verify with device code or use Offline.
      </div>
      <div style={{
        display: 'grid',
        gridTemplateColumns: 'repeat(auto-fit, minmax(124px, 1fr))',
        gap: 12,
        alignItems: 'start',
      }}>
        <ProfileMetaValue label="Name" value={profile?.name || 'Not reported'} />
        <ProfileMetaValue label="Profile UUID" value={profile?.id ? shortenUuid(profile.id) : 'Not reported'} />
        <ProfileMetaValue
          label="Profile"
          value={readinessValue(status.minecraft_profile_ready, 'Ready', 'Not verified')}
        />
        <ProfileMetaValue
          label="Ownership"
          value={readinessValue(status.minecraft_ownership_verified, 'Verified', 'Not verified')}
        />
        <ProfileMetaValue label="Skins" value={skinSummary} />
        <ProfileMetaValue label="Capes" value={profile ? String(profile.capes.length) : 'Not reported'} />
        <ProfileMetaValue label="Verification window" value={verificationWindow} />
      </div>
    </div>
  );
}

function DeviceCodePanel({
  login,
  pollHint,
}: {
  login: AuthLoginPending;
  pollHint?: string | null;
}): JSX.Element {
  const theme = useTheme();
  const [copied, setCopied] = useState<CopyTarget | null>(null);
  const [copyFailed, setCopyFailed] = useState<CopyTarget | null>(null);

  const copy = async (target: CopyTarget, value: string): Promise<void> => {
    setCopied(null);
    setCopyFailed(null);
    try {
      await copyText(value);
      setCopied(target);
    } catch {
      setCopyFailed(target);
    }
  };

  return (
    <div style={{
      display: 'grid',
      gap: 12,
      padding: '14px 16px',
      border: '1px solid var(--line)',
      borderRadius: theme.r.md,
      background: theme.n.surface2,
    }}>
      <div style={{
        display: 'flex',
        alignItems: 'flex-start',
        justifyContent: 'space-between',
        gap: 14,
        flexWrap: 'wrap',
      }}>
        <div style={{ display: 'grid', gap: 5, minWidth: 220 }}>
          <div style={{
            fontSize: 11,
            fontWeight: 700,
            color: theme.n.textMute,
            textTransform: 'uppercase',
            letterSpacing: 0,
          }}>Microsoft code</div>
          <div style={{
            color: theme.n.text,
            fontFamily: theme.font.sans,
            fontSize: 22,
            fontWeight: 800,
            letterSpacing: 0,
            lineHeight: 1.1,
          }}>{login.user_code}</div>
        </div>
        <Button
          variant="secondary"
          size="sm"
          icon={copied === 'code' ? 'check' : 'copy'}
          onClick={() => void copy('code', login.user_code)}
          sound="affirm"
          title="Copy Microsoft code"
        >
          {copied === 'code' ? 'Copied' : 'Copy code'}
        </Button>
      </div>

      <div style={{ display: 'grid', gap: 6 }}>
        <div style={{
          fontSize: 12,
          color: theme.n.textDim,
          lineHeight: 1.45,
        }}>
          {login.message || 'Enter this code at the Microsoft verification page.'}
          {' '}Profile verification may complete after sign-in. Online launch mode can be selected only while the verified account credentials remain valid.
        </div>
        <div style={{
          display: 'flex',
          alignItems: 'center',
          gap: 8,
          flexWrap: 'wrap',
        }}>
          <a
            href={login.verification_uri}
            target="_blank"
            rel="noreferrer"
            style={{
              color: theme.n.text,
              fontSize: 13,
              fontWeight: 650,
              overflowWrap: 'anywhere',
            }}
          >
            {login.verification_uri}
          </a>
          <Button
            variant="secondary"
            size="sm"
            icon={copied === 'url' ? 'check' : 'copy'}
            onClick={() => void copy('url', login.verification_uri)}
            sound="affirm"
            title="Copy verification URL"
          >
            {copied === 'url' ? 'Copied' : 'Copy URL'}
          </Button>
        </div>
      </div>

      <div style={{
        display: 'flex',
        gap: 12,
        flexWrap: 'wrap',
        color: theme.n.textMute,
        fontSize: 11,
        fontWeight: 600,
      }}>
        <span>Expires in {formatSeconds(login.expires_in)}</span>
        <span>Poll interval {formatSeconds(login.interval)}</span>
        <span>{pollHint || 'Waiting for approval'}</span>
      </div>

      {copyFailed && (
        <div style={{
          color: 'var(--err)',
          fontSize: 12,
          fontWeight: 500,
        }}>
          Copy failed. Select the {copyFailed === 'code' ? 'code' : 'URL'} and copy it manually.
        </div>
      )}
    </div>
  );
}

function AccountBoundary({ savedUsername }: { savedUsername: string }): JSX.Element {
  const theme = useTheme();
  const { status, state, refresh: refreshStatus } = useAuthStatus(savedUsername);
  const [login, setLogin] = useState<AuthLoginPending | null>(null);
  const [loginBusy, setLoginBusy] = useState(false);
  const [loginError, setLoginError] = useState<string | null>(null);
  const [pollHint, setPollHint] = useState<string | null>(null);
  const [loginSuccess, setLoginSuccess] = useState<string | null>(null);
  const [logoutBusy, setLogoutBusy] = useState(false);
  const [logoutError, setLogoutError] = useState<string | null>(null);
  const [refreshBusy, setRefreshBusy] = useState(false);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [refreshSuccess, setRefreshSuccess] = useState<string | null>(null);
  const statusLabel = state === 'ready' && status
    ? status.mode === 'offline' ? 'Offline' : status.mode
    : state === 'loading' ? 'Loading' : 'Unavailable';
  const statusTone = state === 'ready' ? 'info' : 'neutral';

  useEffect(() => {
    setLogin(null);
    setLoginError(null);
    setLoginBusy(false);
    setPollHint(null);
    setLoginSuccess(null);
    setLogoutBusy(false);
    setLogoutError(null);
    setRefreshBusy(false);
    setRefreshError(null);
    setRefreshSuccess(null);
  }, [savedUsername]);

  useEffect(() => {
    if (!login) return undefined;
    let active = true;
    const timeout = window.setTimeout(() => {
      void api('POST', `/auth/login/${encodeURIComponent(login.login_id)}/poll`)
        .then((response: unknown) => {
          if (!active) return;
          const poll = pollResponse(response);
          if (!poll) {
            setLogin(null);
            setPollHint(null);
            setLoginError(pollTerminalMessage(null));
            return;
          }

          if (poll.status === 'pending') {
            setPollHint(poll.poll_hint ? boundedMessage(poll.poll_hint, '') : null);
            setLogin((current) => current?.login_id === login.login_id
              ? { ...current, interval: poll.interval }
              : current);
            return;
          }

          if (poll.status === 'msa_authenticated') {
            setLogin(null);
            setPollHint(null);
            setLoginError(null);
            setLoginSuccess(pollSuccessMessage(poll));
            refreshStatus();
            return;
          }

          setLogin(null);
          setPollHint(null);
          setLoginError(pollTerminalMessage(poll));
        })
        .catch((err: unknown) => {
          if (!active) return;
          setLogin(null);
          setPollHint(null);
          if (isApiError(err)) {
            setLoginError(pollErrorMessage(err.payload));
            return;
          }
          setLoginError('Could not reach the local backend while polling Microsoft sign-in.');
        });
    }, Math.max(1, login.interval) * 1000);

    return () => {
      active = false;
      window.clearTimeout(timeout);
    };
  }, [login, refreshStatus]);

  const startLogin = async (): Promise<void> => {
    if (loginBusy || logoutBusy || refreshBusy) return;
    setLoginBusy(true);
    setLogin(null);
    setLoginError(null);
    setLogoutError(null);
    setLoginSuccess(null);
    setRefreshError(null);
    setRefreshSuccess(null);
    setPollHint(null);
    try {
      const response = await api('POST', '/auth/login');
      const pending = loginPendingResponse(response);
      if (pending) {
        setLogin(pending);
        return;
      }
      setLogin(null);
      setLoginError(loginErrorMessage(response));
    } catch (err: unknown) {
      setLogin(null);
      if (isApiError(err)) {
        setLoginError(loginErrorMessage(err.payload));
        return;
      }
      setLoginError('Could not reach the local backend.');
    } finally {
      setLoginBusy(false);
    }
  };

  const logout = async (): Promise<void> => {
    if (logoutBusy || loginBusy || refreshBusy) return;
    setLogoutBusy(true);
    setLogin(null);
    setPollHint(null);
    setLoginError(null);
    setLogoutError(null);
    setLoginSuccess(null);
    setRefreshError(null);
    setRefreshSuccess(null);
    try {
      const response = await api('POST', '/auth/logout');
      if (typeof response === 'object' && response !== null && typeof response.error === 'string') {
        setLogoutError(logoutErrorMessage(response));
      } else {
        setLoginSuccess('Microsoft sign-in was cleared. Switch to Offline for the reliable launch path if Online was selected.');
      }
    } catch (err: unknown) {
      setLogoutError(isApiError(err)
        ? logoutErrorMessage(err.payload)
        : 'Could not reach the local backend to clear Microsoft sign-in.');
    } finally {
      refreshStatus();
      setLogoutBusy(false);
    }
  };

  const refreshAuth = async (): Promise<void> => {
    if (refreshBusy || loginBusy || logoutBusy || login) return;
    setRefreshBusy(true);
    setLogin(null);
    setPollHint(null);
    setLoginError(null);
    setLogoutError(null);
    setLoginSuccess(null);
    setRefreshError(null);
    setRefreshSuccess(null);
    try {
      const response = await api('POST', '/auth/refresh');
      if (isRecord(response) && typeof response.error === 'string') {
        setRefreshError(authRefreshErrorMessage(response));
        return;
      }
      if (!isRecord(response) || response.status !== 'refreshed') {
        setRefreshError('Microsoft sign-in refresh returned an unexpected response. Re-verify with Microsoft or use Offline.');
        return;
      }
      setRefreshSuccess(authRefreshSuccessMessage(response));
    } catch (err: unknown) {
      setRefreshError(isApiError(err)
        ? authRefreshErrorMessage(err.payload)
        : 'Could not reach the local backend to refresh Microsoft sign-in.');
    } finally {
      refreshStatus();
      setRefreshBusy(false);
    }
  };

  const msaActive = Boolean(status?.msa_authenticated);
  const minecraftVerified = Boolean(status?.minecraft_profile_ready === true && status?.minecraft_ownership_verified === true);
  const minecraftReadinessReported = status ? hasMinecraftReadiness(status) : false;
  const minecraftCredentialsActive = Boolean(status && (
    status.minecraft_profile_ready === true ||
    status.minecraft_ownership_verified === true ||
    typeof status.minecraft_token_expires_in === 'number'
  ));
  const selectedAuthMode = status
    ? launchAuthMode(config.value?.launch_auth_mode ?? status.launch_auth_mode)
    : 'offline';
  const onlineSelectable = status ? statusCanSelectOnline(status) : false;
  const effectiveModeLabel = status?.mode === 'online' ? 'online' : 'offline';
  const refreshAvailable = Boolean(status?.login_available && status?.msa_refresh_available);
  const statusCopy = msaActive
    ? minecraftVerified
      ? `Microsoft sign-in and Minecraft profile are verified. Croopor launches as ${status?.username} when Online is selected and these credentials remain inside their expiry windows.`
      : minecraftReadinessReported
        ? `Microsoft sign-in is active, but Minecraft profile ownership is not ready. Refresh if available, re-verify with device code, or switch to Offline.`
        : `Microsoft sign-in is active. Online launch still needs Minecraft profile and ownership readiness; re-verify or use Offline.`
    : `Croopor is using ${status?.username} as the current ${effectiveModeLabel} identity. Online launch credentials are ${status?.online_mode_ready ? 'reported ready by the backend' : 'not ready'}.`;
  const actionGuidance = msaActive
    ? status?.login_available
      ? refreshAvailable
        ? "Refresh uses Croopor's securely stored Microsoft auth snapshot when available. Re-verify starts a new device-code sign-in."
        : 'Re-verify starts a new Microsoft device-code sign-in. Use it when refresh is unavailable or rejected.'
      : 'Logout clears Microsoft sign-in state. Re-verification is unavailable right now.'
    : refreshAvailable
      ? "Refresh uses Croopor's securely stored Microsoft auth snapshot when available. Device-code sign-in remains available if refresh is rejected."
      : 'Microsoft sign-in prepares Online launch mode. It does not switch launch mode by itself.';
  const readinessGuidance = state === 'ready' && status
    ? selectedAuthMode === 'online' && !onlineSelectable
      ? status.login_available
        ? refreshAvailable
          ? `${status.login_reason}. Online is selected, but readiness is missing or expired. Refresh, re-verify with device code, or switch to Offline.`
          : `${status.login_reason}. Online is selected, but readiness is missing or expired. Re-verify with device code, or switch to Offline.`
        : `${status.login_reason}. Online is selected, but readiness is missing or expired and sign-in is unavailable. Switch to Offline.`
      : `${status.login_reason}. Offline launches remain available for singleplayer and offline-mode servers.`
    : 'Microsoft sign-in status will appear here when the backend is reachable.';

  return (
    <Card>
      <SectionHeading
        title="Minecraft account"
        right={(
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
            <Pill tone={statusTone} icon="user">{statusLabel}</Pill>
            {msaActive && <Pill tone="ok" icon="check-circle">Microsoft active</Pill>}
            {minecraftVerified && <Pill tone="ok" icon="shield-check">Minecraft verified</Pill>}
            {refreshAvailable && <Pill tone="info" icon="refresh">Refresh available</Pill>}
            {msaActive && (
              <Pill tone={expiryWindowTone(status?.msa_token_expires_in)} icon="clock">
                {compactExpiryWindow('MS', status?.msa_token_expires_in)}
              </Pill>
            )}
            {minecraftCredentialsActive && (
              <Pill tone={expiryWindowTone(status?.minecraft_token_expires_in)} icon="clock">
                {compactExpiryWindow('MC', status?.minecraft_token_expires_in)}
              </Pill>
            )}
          </div>
        )}
      />
      <div style={{ display: 'grid', gap: 12 }}>
        {state === 'ready' && status ? (
          <>
            <div style={{ fontSize: 13, color: theme.n.textDim, lineHeight: 1.5, maxWidth: 780 }}>
              {statusCopy}
            </div>
            <AuthModeControl status={status} onSaved={refreshStatus} />
            <details style={{
              display: 'grid',
              gap: 10,
              paddingTop: 2,
            }}>
              <summary style={{
                width: 'max-content',
                maxWidth: '100%',
                color: theme.n.textMute,
                fontSize: 12,
                fontWeight: 650,
                cursor: 'pointer',
                listStylePosition: 'inside',
              }}>
                Profile details
              </summary>
              <div style={{
                display: 'grid',
                gridTemplateColumns: 'repeat(auto-fit, minmax(124px, 1fr))',
                gap: 12,
                alignItems: 'start',
                paddingTop: 10,
              }}>
                <ProfileMetaValue label="Identity" value={status.mode === 'online' ? 'Online profile' : 'Offline profile'} />
                <ProfileMetaValue label="Verified" value={status.verified ? 'Yes' : 'No'} />
                <ProfileMetaValue label="UUID" value={shortenUuid(status.uuid)} />
                <ProfileMetaValue label="Skin" value={skinSourceLabel(status.skin_source, status.launch_auth_mode)} />
                <ProfileMetaValue label="Login" value={status.login_available ? 'Available' : 'Unavailable'} />
                <ProfileMetaValue
                  label="Refresh"
                  value={status.msa_refresh_available
                    ? status.login_available ? 'Available' : 'Sign-in unavailable'
                    : 'Unavailable'}
                />
                <ProfileMetaValue label="Microsoft" value={msaActive ? 'Active' : 'Inactive'} />
                {msaActive && (
                  <ProfileMetaValue label="MS window" value={expiryWindowValue(status.msa_token_expires_in)} />
                )}
                {minecraftCredentialsActive && (
                  <ProfileMetaValue label="MC window" value={expiryWindowValue(status.minecraft_token_expires_in)} />
                )}
              </div>
            </details>
            <MinecraftProfileReadiness status={status} />
            <div style={{
              display: 'flex',
              alignItems: 'center',
              gap: 10,
              flexWrap: 'wrap',
            }}>
              {refreshAvailable && (
                <Button
                  variant="secondary"
                  icon="refresh"
                  onClick={() => void refreshAuth()}
                  disabled={refreshBusy || loginBusy || logoutBusy || Boolean(login)}
                  sound="affirm"
                >
                  {refreshBusy ? 'Refreshing' : 'Refresh'}
                </Button>
              )}
              {msaActive ? (
                <>
                  {status.login_available && (
                    <Button
                      variant="secondary"
                      icon="globe"
                      onClick={() => void startLogin()}
                      disabled={loginBusy || refreshBusy}
                      sound="affirm"
                    >
                      {loginBusy ? 'Getting code' : 'Re-verify'}
                    </Button>
                  )}
                  <Button
                    variant="secondary"
                    icon="x"
                    onClick={() => void logout()}
                    disabled={logoutBusy || refreshBusy}
                    sound="affirm"
                  >
                    {logoutBusy ? 'Signing out' : 'Log out'}
                  </Button>
                </>
              ) : status.login_available ? (
                <Button
                  variant="secondary"
                  icon="globe"
                  onClick={() => void startLogin()}
                  disabled={loginBusy || refreshBusy}
                  sound="affirm"
                >
                  {loginBusy ? 'Getting code' : 'Get code'}
                </Button>
              ) : (
                <Button
                  variant="secondary"
                  icon="globe"
                  disabled
                  title={status.login_reason}
                >
                  Sign in unavailable
                </Button>
              )}
              <span style={{
                color: theme.n.textMute,
                fontSize: 12,
                lineHeight: 1.4,
              }}>
                {actionGuidance}
              </span>
            </div>
            {login && <DeviceCodePanel login={login} pollHint={pollHint} />}
            {loginSuccess && (
              <div style={{
                color: theme.n.textDim,
                fontSize: 12,
                fontWeight: 500,
                lineHeight: 1.4,
              }}>
                {loginSuccess}
              </div>
            )}
            {refreshSuccess && (
              <div style={{
                color: theme.n.textDim,
                fontSize: 12,
                fontWeight: 500,
                lineHeight: 1.4,
              }}>
                {refreshSuccess}
              </div>
            )}
            {loginError && (
              <div style={{
                color: 'var(--err)',
                fontSize: 12,
                fontWeight: 500,
                lineHeight: 1.4,
              }}>
                {loginError}
              </div>
            )}
            {refreshError && (
              <div style={{
                color: 'var(--err)',
                fontSize: 12,
                fontWeight: 500,
                lineHeight: 1.4,
              }}>
                {refreshError}
              </div>
            )}
            {logoutError && (
              <div style={{
                color: 'var(--err)',
                fontSize: 12,
                fontWeight: 500,
                lineHeight: 1.4,
              }}>
                {logoutError}
              </div>
            )}
          </>
        ) : (
          <div style={{ fontSize: 13, color: theme.n.textDim, lineHeight: 1.5, maxWidth: 780 }}>
            {state === 'loading'
              ? 'Loading account status from the local backend.'
              : 'Account status is unavailable. Offline launch settings are unchanged.'}
          </div>
        )}
        <div style={{
          display: 'flex',
          alignItems: 'flex-start',
          gap: 10,
          padding: '12px 14px',
          border: '1px solid var(--line)',
          borderRadius: theme.r.md,
          background: theme.n.surface2,
          color: theme.n.textDim,
          fontSize: 12,
          lineHeight: 1.45,
        }}>
          <Icon name="shield-check" size={16} color={theme.n.textMute} style={{ marginTop: 1 }} />
          <div>
            {readinessGuidance}
          </div>
        </div>
      </div>
    </Card>
  );
}

function SkinRestorerHelper({ savedUsername }: { savedUsername: string }): JSX.Element {
  const theme = useTheme();
  const [skinUsername, setSkinUsername] = useState(savedUsername);
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle');
  const trimmed = skinUsername.trim();
  const usernameError = trimmed.length > 0 ? validateUsername(trimmed) : null;
  const canCopy = trimmed.length > 0 && usernameError === null;
  const command = `/skin set ${canCopy ? trimmed : '<username>'}`;

  const copyCommand = async (): Promise<void> => {
    if (!canCopy) return;
    setCopyState('idle');
    try {
      await copyText(command);
      setCopyState('copied');
    } catch {
      setCopyState('failed');
    }
  };

  return (
    <Card>
      <SectionHeading
        title="Server skin helper"
        right={<Pill tone="neutral" icon="terminal">SkinRestorer</Pill>}
      />
      <details style={{
        display: 'grid',
        gap: 10,
        paddingTop: 2,
      }}>
        <summary style={{
          width: 'max-content',
          maxWidth: '100%',
          color: theme.n.textMute,
          fontSize: 12,
          fontWeight: 650,
          cursor: 'pointer',
          listStylePosition: 'inside',
        }}>
          SkinRestorer command helper
        </summary>
        <div style={{ display: 'grid', gap: 14, paddingTop: 10 }}>
          <div style={{ fontSize: 13, color: theme.n.textDim, lineHeight: 1.5, maxWidth: 760 }}>
            For servers that use SkinRestorer, copy a command that points your server skin at a Minecraft username. This is a server-side command helper only; Croopor does not upload skins or contact skin services from this page.
          </div>

          <div style={{
            display: 'flex',
            gap: 10,
            flexWrap: 'wrap',
            alignItems: 'end',
          }}>
            <label style={{ display: 'grid', gap: 6, flex: '1 1 260px', maxWidth: 360, minWidth: 220 }}>
              <span style={{
                fontSize: 11,
                fontWeight: 600,
                color: theme.n.textMute,
                textTransform: 'uppercase',
                letterSpacing: 0,
              }}>Skin username</span>
              <Input
                value={skinUsername}
                onChange={(v) => {
                  setSkinUsername(clampPlayerNameInput(v));
                  setCopyState('idle');
                }}
                placeholder={savedUsername}
                icon="user"
              />
            </label>

            <div style={{ display: 'grid', gap: 6, flex: '999 1 320px', minWidth: 240 }}>
              <div style={{
                fontSize: 11,
                fontWeight: 600,
                color: theme.n.textMute,
                textTransform: 'uppercase',
                letterSpacing: 0,
              }}>Command</div>
              <div
                aria-label="SkinRestorer command"
                style={{
                  minHeight: 38,
                  display: 'flex',
                  alignItems: 'center',
                  padding: '9px 12px',
                  border: '1px solid var(--line)',
                  borderRadius: theme.r.md,
                  background: theme.n.surface2,
                  color: canCopy ? theme.n.text : theme.n.textMute,
                  fontFamily: theme.font.sans,
                  fontSize: 12,
                  lineHeight: 1.45,
                  overflowWrap: 'anywhere',
                  userSelect: 'text',
                  cursor: 'text',
                }}
              >
                {command}
              </div>
            </div>

            <Button
              onClick={() => void copyCommand()}
              disabled={!canCopy}
              variant="secondary"
              icon={copyState === 'copied' ? 'check' : 'copy'}
              sound="affirm"
              title="Copy SkinRestorer command"
            >
              {copyState === 'copied' ? 'Copied' : 'Copy'}
            </Button>
          </div>

          {(usernameError || copyState === 'failed') && (
            <div style={{
              fontSize: 12,
              fontWeight: 500,
              color: copyState === 'failed' ? 'var(--err)' : theme.n.textMute,
              lineHeight: 1.4,
            }}>
              {copyState === 'failed'
                ? 'Copy failed. Select the command and copy it manually.'
                : usernameError}
            </div>
          )}
        </div>
      </details>
    </Card>
  );
}

function menuItemsForSavedSkin({
  skin,
  selectedPreviewEditing,
  onlineReady,
  applying,
  deleting,
  onView,
  onApply,
  onEdit,
  onDownload,
  onDelete,
}: {
  skin: SavedSkinRecord;
  selectedPreviewEditing: boolean;
  onlineReady: boolean;
  applying: boolean;
  deleting: boolean;
  onView: () => void;
  onApply: () => void;
  onEdit: () => void;
  onDownload: () => void;
  onDelete: () => void;
}): ContextMenuItem[] {
  const applied = Boolean(skin.applied_at);
  const items: ContextMenuItem[] = [];

  if (!deleting) {
    items.push({ icon: 'image', label: 'View', onSelect: onView });
  }
  if (onlineReady && !applied && !applying) {
    items.push({ icon: 'check', label: 'Apply', onSelect: onApply });
  }
  if (!selectedPreviewEditing && !deleting && !applying) {
    items.push({ icon: 'edit', label: 'Edit', onSelect: onEdit });
  }
  if (!deleting) {
    items.push({ icon: 'download', label: 'Download PNG', onSelect: onDownload });
    items.push({ label: '', onSelect: () => {}, divider: true });
    items.push({ icon: 'trash', label: 'Delete', onSelect: onDelete, danger: true });
  }

  return items;
}

function SavedSkinLibrary({
  onlineReady,
  minecraftProfile,
  onApplied,
}: {
  onlineReady: boolean;
  minecraftProfile?: MinecraftProfile;
  onApplied: () => void;
}): JSX.Element {
  const theme = useTheme();
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const uploadApplyAfterSaveRef = useRef(false);
  const uploadDragDepthRef = useRef(0);
  const stagedUploadUrlRef = useRef<string | null>(null);
  const stagedUploadTokenRef = useRef(0);
  const editDetectTokenRef = useRef(0);
  const { skins, state, error, refresh } = useSavedSkins();
  const [skinName, setSkinName] = useState('');
  const [lookupUsername, setLookupUsername] = useState('');
  const [uploadVariant, setUploadVariant] = useState<UploadSkinVariant>('auto');
  const [stagedUpload, setStagedUpload] = useState<StagedSkinUpload | null>(null);
  const [stagedPreviewSide, setStagedPreviewSide] = useState<SavedSkinPreviewSide>('front');
  const [stagedLayerMode, setStagedLayerMode] = useState<SavedSkinLayerMode>('full');
  const [busy, setBusy] = useState(false);
  const [profileBusy, setProfileBusy] = useState(false);
  const [lookupBusy, setLookupBusy] = useState(false);
  const [uploadDragActive, setUploadDragActive] = useState(false);
  const [uploadHover, setUploadHover] = useState(false);
  const [uploadFocused, setUploadFocused] = useState(false);
  const [message, setMessage] = useState<{
    tone: 'ok' | 'err';
    text: string;
  } | null>(null);
  const [editKey, setEditKey] = useState<string | null>(null);
  const [editName, setEditName] = useState('');
  const [editVariant, setEditVariant] = useState<SkinVariant>('classic');
  const [editBusyKey, setEditBusyKey] = useState<string | null>(null);
  const [editDetectBusyKey, setEditDetectBusyKey] = useState<string | null>(null);
  const [editDetectError, setEditDetectError] = useState<string | null>(null);
  const [deleteKey, setDeleteKey] = useState<string | null>(null);
  const [applyKey, setApplyKey] = useState<string | null>(null);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [sourceDetailsOpen, setSourceDetailsOpen] = useState(false);
  const [skinSort, setSkinSort] = useState<SavedSkinSort>('recent');
  const [skinFilter, setSkinFilter] = useState('');
  const [previewSide, setPreviewSide] = useState<SavedSkinPreviewSide>('front');
  const [previewLayerMode, setPreviewLayerMode] = useState<SavedSkinLayerMode>('full');
  const profileSkin = activeMinecraftSkin(minecraftProfile);
  const profileSkinVariant = skinVariantValue(profileSkin?.variant);
  const trimmedName = skinName.trim();
  const trimmedLookupUsername = lookupUsername.trim();
  const trimmedEditName = editName.trim();
  const lookupUsernameError = trimmedLookupUsername ? validateUsername(trimmedLookupUsername) : null;
  const canUpload = !busy && !profileBusy && !lookupBusy;
  const canSaveProfileSkin = onlineReady && Boolean(profileSkin) && !busy && !profileBusy && !lookupBusy;
  const canSaveLookupSkin = Boolean(trimmedLookupUsername)
    && !lookupUsernameError
    && !busy
    && !profileBusy
    && !lookupBusy;
  const equippedSkin = skins.find((skin) => Boolean(skin.applied_at)) ?? null;
  const selectedSkin = skins.find((skin) => skin.texture_key === selectedKey)
    ?? equippedSkin
    ?? skins[0]
    ?? null;
  const sortedSkins = useMemo(() => sortSavedSkins(skins, skinSort), [skins, skinSort]);
  const normalizedSkinFilter = skinFilter.trim().toLowerCase();
  const visibleSkins = useMemo(
    () => sortedSkins.filter((skin) => savedSkinMatchesFilter(skin, normalizedSkinFilter)),
    [normalizedSkinFilter, sortedSkins],
  );
  const selectedPreviewEditing = Boolean(selectedSkin && editKey === selectedSkin.texture_key);
  const previewPending = Boolean(
    selectedSkin
      && equippedSkin
      && selectedSkin.texture_key !== equippedSkin.texture_key,
  );
  const stagedVariant = stagedUpload ? stagedSkinVariant(stagedUpload, uploadVariant) : null;
  const stagedName = stagedUpload
    ? trimmedName || uploadSkinName(stagedUpload.file) || 'Uploaded skin'
    : '';
  const stagedVariantReady = Boolean(
    stagedUpload && (uploadVariant !== 'auto' || !stagedUpload.detectingVariant),
  );
  const stagedValidated = stagedUpload?.normalizeStatus === 'ready';
  const stagedCanSave = Boolean(stagedUpload && canUpload && stagedVariantReady && stagedValidated);
  const stagedVariantLabel = stagedUpload && stagedVariant
    ? uploadVariant === 'auto'
      ? stagedUpload.detectingVariant
        ? 'Detecting model'
        : `Inferred ${stagedVariant}`
      : `Manual ${stagedVariant}`
    : '';
  const stagedValidationLabel = stagedUpload
    ? stagedUpload.normalizeStatus === 'ready'
      ? 'Validated'
      : stagedUpload.normalizeStatus === 'error'
        ? 'Invalid skin'
        : 'Validating'
    : '';
  const sourceControlsNeedAttention = (state === 'ready' && skins.length === 0)
    || Boolean(stagedUpload)
    || busy
    || profileBusy
    || lookupBusy
    || uploadDragActive
    || message?.tone === 'err';

  const clearStagedUpload = (): void => {
    stagedUploadTokenRef.current += 1;
    if (stagedUploadUrlRef.current) {
      URL.revokeObjectURL(stagedUploadUrlRef.current);
      stagedUploadUrlRef.current = null;
    }
    setStagedUpload(null);
    setStagedPreviewSide('front');
    setStagedLayerMode('full');
    uploadApplyAfterSaveRef.current = false;
    if (fileInputRef.current) fileInputRef.current.value = '';
  };

  useEffect(() => {
    if (state !== 'ready') return;
    if (skins.length === 0) {
      if (selectedKey !== null) setSelectedKey(null);
      return;
    }
    if (selectedKey && skins.some((skin) => skin.texture_key === selectedKey)) return;
    const next = skins.find((skin) => Boolean(skin.applied_at)) ?? skins[0];
    setSelectedKey(next.texture_key);
  }, [skins, selectedKey, state]);

  useEffect(() => {
    if (sourceControlsNeedAttention && !sourceDetailsOpen) {
      setSourceDetailsOpen(true);
    }
  }, [sourceControlsNeedAttention, sourceDetailsOpen]);

  useEffect(() => {
    return () => {
      stagedUploadTokenRef.current += 1;
      if (stagedUploadUrlRef.current) {
        URL.revokeObjectURL(stagedUploadUrlRef.current);
        stagedUploadUrlRef.current = null;
      }
    };
  }, []);

  const upload = async (
    file: File,
    applyAfterSave = false,
    variantOverride?: SkinVariant,
  ): Promise<void> => {
    const name = trimmedName || uploadSkinName(file);
    if (!name) {
      setMessage({ tone: 'err', text: 'Name the skin before uploading.' });
      return;
    }

    setBusy(true);
    setMessage(null);
    try {
      const resolvedVariant = variantOverride ?? await resolveUploadSkinVariant(file, uploadVariant);
      const params = new URLSearchParams({ name, variant: resolvedVariant });
      const response = await fetch(apiUrl(`/skins?${params.toString()}`), {
        method: 'POST',
        headers: { 'Content-Type': 'image/png' },
        body: file,
      });
      const payload = await response.json().catch(() => undefined);
      if (!response.ok) {
        throw new Error(
          readApiPayloadMessage(payload, `Upload failed with HTTP ${response.status}`),
        );
      }
      const saved = savedSkinRecord(payload);
      setSkinName('');
      clearStagedUpload();
      if (saved) setSelectedKey(saved.texture_key);
      if (saved && applyAfterSave) {
        try {
          await applySavedSkin(saved.texture_key);
          setMessage({ tone: 'ok', text: 'Skin saved and applied to Minecraft profile.' });
        } catch (err) {
          setSelectedKey(saved.texture_key);
          refresh();
          setMessage({ tone: 'err', text: savedSkinApplyErrorMessage(err) });
        }
      } else {
        refresh();
      }
    } catch (err) {
      setMessage({
        tone: 'err',
        text: err instanceof Error ? err.message : 'Could not save skin.',
      });
    } finally {
      setBusy(false);
      if (fileInputRef.current) fileInputRef.current.value = '';
    }
  };

  const deleteSkin = async (textureKey: string): Promise<void> => {
    setDeleteKey(textureKey);
    setMessage(null);
    try {
      await api('DELETE', `/skins/${textureKey}`);
      if (selectedKey === textureKey) setSelectedKey(null);
      refresh();
    } catch (err) {
      setMessage({
        tone: 'err',
        text: err instanceof Error ? err.message : 'Could not delete skin.',
      });
    } finally {
      setDeleteKey(null);
    }
  };

  const confirmDeleteSkin = async (skin: SavedSkinRecord): Promise<void> => {
    const name = skin.name.trim();
    const ok = await showConfirm(
      name
        ? `Delete saved skin "${name}"? This removes it from local saved skins only.`
        : 'Delete this saved skin? This removes it from local saved skins only.',
      { title: 'Delete saved skin', destructive: true, confirmText: 'Delete' },
    );
    if (!ok) return;
    await deleteSkin(skin.texture_key);
  };

  const saveProfileSkin = async (): Promise<void> => {
    if (!onlineReady) return;

    setProfileBusy(true);
    setMessage(null);
    try {
      const payload = await api('POST', '/skins/from-profile', {});
      const saved = savedSkinRecord(payload);
      if (saved) setSelectedKey(saved.texture_key);
      refresh();
      setMessage({ tone: 'ok', text: 'Minecraft profile skin saved.' });
    } catch (err) {
      setMessage({
        tone: 'err',
        text: err instanceof Error ? err.message : 'Could not save Minecraft profile skin.',
      });
    } finally {
      setProfileBusy(false);
    }
  };

  const saveUsernameSkin = async (applyAfterSave: boolean): Promise<void> => {
    if (!trimmedLookupUsername) {
      setMessage({ tone: 'err', text: 'Enter a Minecraft username.' });
      return;
    }
    if (lookupUsernameError) {
      setMessage({ tone: 'err', text: lookupUsernameError });
      return;
    }

    setLookupBusy(true);
    setMessage(null);
    try {
      const payload = await api('POST', '/skins/from-username', {
        username: trimmedLookupUsername,
      });
      const saved = savedSkinRecord(payload);
      if (saved) setSelectedKey(saved.texture_key);
      setLookupUsername('');
      if (saved && applyAfterSave) {
        try {
          await applySavedSkin(saved.texture_key);
          setMessage({ tone: 'ok', text: 'Player skin saved and applied to Minecraft profile.' });
        } catch (err) {
          setSelectedKey(saved.texture_key);
          refresh();
          setMessage({ tone: 'err', text: savedSkinApplyErrorMessage(err) });
        }
      } else {
        refresh();
        setMessage({ tone: 'ok', text: 'Player skin saved.' });
      }
    } catch (err) {
      setMessage({
        tone: 'err',
        text: err instanceof Error ? err.message : 'Could not save player skin.',
      });
    } finally {
      setLookupBusy(false);
    }
  };

  const startEdit = (skin: SavedSkinRecord): void => {
    editDetectTokenRef.current += 1;
    setEditKey(skin.texture_key);
    setEditName(skin.name);
    setEditVariant(skin.variant === 'slim' ? 'slim' : 'classic');
    setEditDetectBusyKey(null);
    setEditDetectError(null);
    setMessage(null);
  };

  const cancelEdit = (): void => {
    editDetectTokenRef.current += 1;
    setEditKey(null);
    setEditName('');
    setEditVariant('classic');
    setEditDetectBusyKey(null);
    setEditDetectError(null);
  };

  const detectSavedSkinModel = async (skin: SavedSkinRecord): Promise<void> => {
    const token = editDetectTokenRef.current + 1;
    editDetectTokenRef.current = token;
    setEditDetectBusyKey(skin.texture_key);
    setEditDetectError(null);
    setMessage(null);
    try {
      const detectedVariant = await detectSkinVariantFromSavedSkin(skin);
      if (token !== editDetectTokenRef.current) return;
      setEditVariant(detectedVariant);
    } catch (err) {
      if (token !== editDetectTokenRef.current) return;
      setEditDetectError(
        boundedMessage(err instanceof Error ? err.message : undefined, 'Could not detect skin model.'),
      );
    } finally {
      if (token === editDetectTokenRef.current) setEditDetectBusyKey(null);
    }
  };

  const saveSkinMetadata = async (textureKey: string): Promise<void> => {
    if (!trimmedEditName) {
      setMessage({ tone: 'err', text: 'Name the skin before saving.' });
      return;
    }

    setEditBusyKey(textureKey);
    setMessage(null);
    try {
      await api('PUT', `/skins/${textureKey}`, {
        name: trimmedEditName,
        variant: editVariant,
      });
      cancelEdit();
      refresh();
      setMessage({ tone: 'ok', text: 'Skin details updated.' });
    } catch (err) {
      setMessage({
        tone: 'err',
        text: err instanceof Error ? err.message : 'Could not update skin details.',
      });
    } finally {
      setEditBusyKey(null);
    }
  };

  const applySavedSkin = async (textureKey: string): Promise<void> => {
    await api('POST', `/skins/${textureKey}/apply`);
    onApplied();
    setSelectedKey(textureKey);
    refresh();
  };

  const applySkin = async (textureKey: string): Promise<void> => {
    const skin = skins.find((saved) => saved.texture_key === textureKey);
    if (skin?.applied_at) return;

    setApplyKey(textureKey);
    setMessage(null);
    try {
      await applySavedSkin(textureKey);
      setMessage({ tone: 'ok', text: 'Skin applied to Minecraft profile.' });
    } catch (err) {
      setMessage({
        tone: 'err',
        text: err instanceof Error ? err.message : 'Could not apply skin.',
      });
    } finally {
      setApplyKey(null);
    }
  };

  const downloadSavedSkin = (skin: SavedSkinRecord): void => {
    const anchor = document.createElement('a');
    anchor.href = savedSkinFileUrl(skin);
    anchor.download = savedSkinDownloadFilename(skin);
    anchor.style.display = 'none';
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
  };

  const resetPreview = (): void => {
    if (!equippedSkin) return;
    setSelectedKey(equippedSkin.texture_key);
    setMessage(null);
  };

  const stageUploadFile = (file: File, applyAfterSave: boolean): void => {
    if (!isPngFile(file)) {
      setMessage({ tone: 'err', text: 'Upload a PNG skin file.' });
      if (fileInputRef.current) fileInputRef.current.value = '';
      return;
    }
    if (busy || profileBusy || lookupBusy) {
      setMessage({ tone: 'err', text: 'Wait for the current skin action to finish.' });
      if (fileInputRef.current) fileInputRef.current.value = '';
      return;
    }

    const objectUrl = URL.createObjectURL(file);
    stagedUploadTokenRef.current += 1;
    const token = stagedUploadTokenRef.current;
    if (stagedUploadUrlRef.current) URL.revokeObjectURL(stagedUploadUrlRef.current);
    stagedUploadUrlRef.current = objectUrl;
    setMessage(null);
    setStagedPreviewSide('front');
    setStagedLayerMode('full');
    setStagedUpload({
      file,
      objectUrl,
      detectedVariant: 'classic',
      detectingVariant: true,
      normalizeStatus: 'checking',
      applyAfterSave,
    });

    void normalizeSkinUpload(file).then((metadata) => {
      if (token !== stagedUploadTokenRef.current) return;
      setStagedUpload((current) => current?.objectUrl === objectUrl
        ? {
            ...current,
            detectedVariant: metadata.variantSuggestion,
            detectingVariant: false,
            normalizeStatus: 'ready',
            normalizeError: undefined,
            textureKey: metadata.textureKey,
            originalWidth: metadata.originalWidth,
            originalHeight: metadata.originalHeight,
            normalizedByteSize: metadata.normalizedByteSize,
          }
        : current);
    }).catch((err) => {
      if (token !== stagedUploadTokenRef.current) return;
      setStagedUpload((current) => current?.objectUrl === objectUrl
        ? {
            ...current,
            detectingVariant: false,
            normalizeStatus: 'error',
            normalizeError: boundedMessage(err instanceof Error ? err.message : undefined, 'Skin validation failed.'),
          }
        : current);
    });
  };

  const saveStagedUpload = (applyAfterSave: boolean): void => {
    if (!stagedUpload || !stagedVariant || !stagedCanSave) return;
    if (applyAfterSave && !onlineReady) return;
    void upload(stagedUpload.file, applyAfterSave, stagedVariant);
  };

  const handleUploadDrop = (event: DragEvent): void => {
    event.preventDefault();
    event.stopPropagation();
    uploadDragDepthRef.current = 0;
    setUploadDragActive(false);

    const files = event.dataTransfer?.files;
    if (!files || files.length === 0) return;
    if (files.length !== 1) {
      setMessage({ tone: 'err', text: 'Drop one PNG skin file.' });
      return;
    }

    stageUploadFile(files[0], false);
  };

  const handleUploadDragEnter = (event: DragEvent): void => {
    if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    uploadDragDepthRef.current += 1;
    setUploadDragActive(true);
  };

  const handleUploadDragOver = (event: DragEvent): void => {
    if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = busy || profileBusy || lookupBusy ? 'none' : 'copy';
  };

  const handleUploadDragLeave = (event: DragEvent): void => {
    if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    uploadDragDepthRef.current = Math.max(0, uploadDragDepthRef.current - 1);
    if (uploadDragDepthRef.current === 0) setUploadDragActive(false);
  };

  const handleUploadFile = (file: File, applyAfterSave: boolean): void => {
    stageUploadFile(file, applyAfterSave);
  };

  const openUploadPicker = (applyAfterSave: boolean): void => {
    uploadApplyAfterSaveRef.current = applyAfterSave;
    if (fileInputRef.current) fileInputRef.current.value = '';
    fileInputRef.current?.click();
  };

  const uploadActive = uploadDragActive || uploadHover || uploadFocused;
  const skinMetadataEditor = (skin: SavedSkinRecord, style: JSX.CSSProperties): JSX.Element => (
    <div style={style}>
      <Input
        value={editName}
        onChange={(value) => setEditName(value.slice(0, 64))}
        icon="tag"
        placeholder="Skin name"
      />
      <Segmented<SkinVariant>
        options={[
          { value: 'classic', label: 'Classic' },
          { value: 'slim', label: 'Slim' },
        ]}
        value={editVariant}
        onChange={(value) => {
          setEditVariant(value);
          setEditDetectError(null);
        }}
      />
      <Button
        variant="ghost"
        size="sm"
        icon={editDetectBusyKey === skin.texture_key ? 'refresh' : 'search'}
        disabled={editBusyKey === skin.texture_key || editDetectBusyKey === skin.texture_key}
        onClick={() => void detectSavedSkinModel(skin)}
        title="Detect model from the saved PNG"
      >
        {editDetectBusyKey === skin.texture_key ? 'Detecting' : 'Detect'}
      </Button>
      <Button
        variant="secondary"
        size="sm"
        icon={editBusyKey === skin.texture_key ? 'refresh' : 'check'}
        disabled={
          editBusyKey === skin.texture_key ||
          editDetectBusyKey === skin.texture_key ||
          trimmedEditName.length === 0
        }
        onClick={() => void saveSkinMetadata(skin.texture_key)}
      >
        Save
      </Button>
      <Button
        variant="ghost"
        size="sm"
        icon="x"
        disabled={editBusyKey === skin.texture_key}
        onClick={cancelEdit}
      >
        Cancel
      </Button>
      {editDetectError && editKey === skin.texture_key && (
        <div style={{
          gridColumn: '1 / -1',
          color: 'var(--err)',
          fontSize: 12,
          fontWeight: 500,
          lineHeight: 1.4,
        }}>
          {editDetectError}
        </div>
      )}
    </div>
  );

  return (
    <Card>
      <SectionHeading
        title="Saved skins"
        right={(
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', justifyContent: 'flex-end' }}>
            <Pill tone="neutral" icon="image">{state === 'ready' ? String(skins.length) : '...'}</Pill>
            <Input
              value={skinFilter}
              onChange={(value) => setSkinFilter(value.slice(0, 80))}
              placeholder="Filter skins"
              icon="search"
              style={{ width: 190 }}
            />
            <div role="group" aria-label="Saved skins sort">
              <Segmented<SavedSkinSort>
                options={[
                  { value: 'recent', label: 'Recent' },
                  { value: 'name', label: 'Name' },
                  { value: 'equipped', label: 'Equipped' },
                  { value: 'source', label: 'Source' },
                ]}
                value={skinSort}
                onChange={setSkinSort}
              />
            </div>
          </div>
        )}
      />
      <div style={{ display: 'grid', gap: 16 }}>
        <details
          open={sourceDetailsOpen}
          onToggle={(event) => setSourceDetailsOpen(event.currentTarget.open)}
          style={{
            borderBottom: '1px solid var(--line)',
            paddingBottom: 12,
          }}
        >
          <summary style={{
            cursor: 'pointer',
            color: theme.n.text,
            fontSize: 13,
            fontWeight: 700,
            lineHeight: 1.4,
          }}>
            Add a skin
          </summary>
          <div style={{ display: 'grid', gap: 12, marginTop: 12 }}>
            <div
              style={{
                display: 'grid',
                gap: 10,
                padding: 12,
                border: uploadActive ? '1px dashed var(--accent-line)' : '1px dashed var(--line)',
                borderRadius: theme.r.md,
                background: uploadActive ? 'var(--accent-softer)' : theme.n.surface2,
                boxShadow: uploadActive ? '0 0 0 3px var(--accent-ring)' : undefined,
              }}
              onDragEnter={handleUploadDragEnter}
              onDragOver={handleUploadDragOver}
              onDragLeave={handleUploadDragLeave}
              onDrop={handleUploadDrop}
              onMouseEnter={() => setUploadHover(true)}
              onMouseLeave={() => setUploadHover(false)}
              onFocusCapture={() => setUploadFocused(true)}
              onBlurCapture={() => setUploadFocused(false)}
            >
              <div style={{
                display: 'flex',
                flexWrap: 'wrap',
                gap: 10,
                alignItems: 'center',
                minWidth: 0,
              }}>
                <Input
                  value={skinName}
                  onChange={(value) => {
                    setSkinName(value.slice(0, 64));
                    setMessage(null);
                  }}
                  placeholder="Skin name, optional"
                  icon="tag"
                  style={{ flex: '1 1 220px', minWidth: 0 }}
                />
                <div style={{ flex: '0 1 auto', minWidth: 0 }}>
                  <Segmented<UploadSkinVariant>
                    options={[
                      { value: 'auto', label: 'Auto' },
                      { value: 'classic', label: 'Classic' },
                      { value: 'slim', label: 'Slim' },
                    ]}
                    value={uploadVariant}
                    onChange={(value) => {
                      setUploadVariant(value);
                      setMessage(null);
                    }}
                  />
                </div>
                <Button
                  variant="secondary"
                  icon="plus"
                  disabled={!canUpload}
                  onClick={() => openUploadPicker(false)}
                >
                  Upload PNG
                </Button>
                <Button
                  variant="ghost"
                  icon="check"
                  disabled={!canUpload || !onlineReady}
                  onClick={() => openUploadPicker(true)}
                  title={onlineReady ? 'Save locally, then apply to the active Minecraft account' : 'Online Minecraft account required'}
                >
                  Upload & apply
                </Button>
              </div>
              <div style={{
                color: uploadDragActive ? 'var(--accent)' : theme.n.textMute,
                fontSize: 12,
                fontWeight: 500,
                lineHeight: 1.4,
              }}>
                {uploadDragActive ? 'Drop one PNG skin file to preview.' : 'Drag a PNG skin file here, or use Upload PNG.'}
              </div>
              {stagedUpload && stagedVariant && (
                <div style={{
                  display: 'grid',
                  gridTemplateColumns: 'auto minmax(0, 1fr)',
                  gap: 14,
                  alignItems: 'center',
                  minWidth: 0,
                  paddingTop: 12,
                  marginTop: 2,
                  borderTop: '1px solid var(--line)',
                }}>
                  <div style={{ display: 'grid', gap: 8, justifyItems: 'center' }}>
                    <SkinBodyPreview
                      src={stagedUpload.objectUrl}
                      name={stagedName}
                      variant={stagedVariant}
                      side={stagedPreviewSide}
                      showOuterLayers={stagedLayerMode === 'full'}
                    />
                    <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', justifyContent: 'center' }}>
                      <div role="group" aria-label={`${stagedName} staged body preview side`}>
                        <Segmented<SavedSkinPreviewSide>
                          options={[
                            { value: 'front', label: 'Front' },
                            { value: 'back', label: 'Back' },
                          ]}
                          value={stagedPreviewSide}
                          onChange={setStagedPreviewSide}
                        />
                      </div>
                      <div role="group" aria-label={`${stagedName} staged body preview layers`}>
                        <Segmented<SavedSkinLayerMode>
                          options={[
                            { value: 'full', label: 'Full' },
                            { value: 'base', label: 'Base' },
                          ]}
                          value={stagedLayerMode}
                          onChange={setStagedLayerMode}
                        />
                      </div>
                    </div>
                  </div>
                  <div style={{ minWidth: 0, display: 'grid', gap: 7 }}>
                    <div style={{ display: 'flex', alignItems: 'center', gap: 7, flexWrap: 'wrap' }}>
                      <Pill tone="info" icon="image">Staged PNG</Pill>
                      <Pill
                        tone={stagedUpload.normalizeStatus === 'ready' ? 'ok' : stagedUpload.normalizeStatus === 'error' ? 'err' : 'neutral'}
                        icon={stagedUpload.normalizeStatus === 'ready' ? 'check-circle' : stagedUpload.normalizeStatus === 'error' ? 'alert' : 'refresh'}
                      >
                        {stagedValidationLabel}
                      </Pill>
                      <Pill tone="neutral">
                        {stagedVariantLabel}
                      </Pill>
                      {stagedUpload.applyAfterSave && (
                        <Pill tone={onlineReady ? 'ok' : 'warn'} icon={onlineReady ? 'check-circle' : 'alert'}>
                          Apply requested
                        </Pill>
                      )}
                    </div>
                    <div style={{
                      color: theme.n.text,
                      fontSize: 14,
                      fontWeight: 700,
                      lineHeight: 1.25,
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                      whiteSpace: 'nowrap',
                    }}>
                      {stagedName}
                    </div>
                    <div style={{
                      color: theme.n.textMute,
                      fontSize: 12,
                      fontFamily: theme.font.sans,
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                      whiteSpace: 'nowrap',
                    }}>
                      {stagedUpload.normalizeStatus === 'ready' && stagedUpload.textureKey
                        ? `${stagedUpload.textureKey.slice(0, 16)} / ${stagedUpload.originalWidth}x${stagedUpload.originalHeight} -> 64x64 / ${formatByteSize(stagedUpload.normalizedByteSize ?? stagedUpload.file.size)}`
                        : `${stagedUpload.file.name} / ${formatByteSize(stagedUpload.file.size)}`}
                    </div>
                    <div style={{ color: theme.n.textDim, fontSize: 12, lineHeight: 1.4, maxWidth: 560 }}>
                      {stagedUpload.normalizeStatus === 'error'
                        ? stagedUpload.normalizeError || 'Skin validation failed.'
                        : stagedUpload.normalizeStatus === 'checking'
                          ? 'Croopor is validating and normalizing this skin before it can be saved.'
                          : 'Review the normalized PNG before saving it to your local skin library.'}
                    </div>
                  </div>
                  <div style={{
                    display: 'flex',
                    gridColumn: '1 / -1',
                    justifyContent: 'flex-end',
                    alignItems: 'center',
                    gap: 8,
                    flexWrap: 'wrap',
                  }}>
                    <Button
                      variant="ghost"
                      size="sm"
                      icon="x"
                      disabled={busy}
                      onClick={clearStagedUpload}
                    >
                      Cancel
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      icon="plus"
                      disabled={!canUpload}
                      onClick={() => openUploadPicker(stagedUpload.applyAfterSave)}
                    >
                      Replace
                    </Button>
                    <Button
                      variant={stagedUpload.applyAfterSave ? 'ghost' : 'secondary'}
                      size="sm"
                      icon={busy ? 'refresh' : 'download'}
                      disabled={!stagedCanSave}
                      onClick={() => saveStagedUpload(false)}
                    >
                      Save locally
                    </Button>
                    <Button
                      variant={stagedUpload.applyAfterSave ? 'secondary' : 'ghost'}
                      size="sm"
                      icon={busy ? 'refresh' : 'check'}
                      disabled={!stagedCanSave || !onlineReady}
                      onClick={() => saveStagedUpload(true)}
                      title={onlineReady ? 'Save locally, then apply to the active Minecraft account' : 'Online Minecraft account required'}
                    >
                      Save & apply
                    </Button>
                  </div>
                </div>
              )}
              <input
                ref={fileInputRef}
                type="file"
                accept="image/png"
                style={{ display: 'none' }}
                onChange={(event) => {
                  const file = event.currentTarget.files?.[0];
                  if (file) handleUploadFile(file, uploadApplyAfterSaveRef.current);
                  uploadApplyAfterSaveRef.current = false;
                }}
              />
            </div>

            {minecraftProfile && (
              <div style={{
                display: 'grid',
                gridTemplateColumns: '42px minmax(0, 1fr) auto',
                gap: 12,
                alignItems: 'center',
                padding: '12px 0',
                borderTop: '1px solid var(--line)',
                borderBottom: '1px solid var(--line)',
              }}>
                {profileSkin ? (
                  <SkinBodyPreview
                    src={profileSkin.url}
                    name={minecraftProfile.name}
                    variant={profileSkinVariant}
                    side="front"
                    showOuterLayers
                    scale={2}
                    compact
                  />
                ) : (
                  <PlayerHeadPreview
                    username={minecraftProfile.name}
                    textureSrc={undefined}
                    size={36}
                    radius={7}
                    ariaLabel={`${minecraftProfile.name} Minecraft profile skin`}
                  />
                )}
                <div style={{ minWidth: 0, display: 'grid', gap: 4 }}>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
                    <Pill tone={onlineReady ? 'ok' : 'warn'} icon={onlineReady ? 'check-circle' : 'alert'}>
                      Current Minecraft profile
                    </Pill>
                    {profileSkin && <Pill tone="neutral">{profileSkinVariant}</Pill>}
                  </div>
                  <div style={{
                    color: theme.n.text,
                    fontSize: 13,
                    fontWeight: 700,
                    lineHeight: 1.25,
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                    whiteSpace: 'nowrap',
                  }}>
                    {minecraftProfile.name}
                  </div>
                  <div style={{ color: theme.n.textMute, fontSize: 12, lineHeight: 1.4 }}>
                    {profileSkin
                      ? 'Save this skin locally before editing, previewing, or reapplying it later.'
                      : 'This profile does not report an active skin texture yet.'}
                  </div>
                </div>
                <Button
                  variant="secondary"
                  size="sm"
                  icon={profileBusy ? 'refresh' : 'download'}
                  disabled={!canSaveProfileSkin}
                  onClick={() => void saveProfileSkin()}
                  title={onlineReady
                    ? profileSkin ? 'Save active Minecraft profile skin' : 'No active Minecraft profile skin reported'
                    : 'Online Minecraft account required'}
                >
                  Save locally
                </Button>
              </div>
            )}

            <div style={{
              display: 'grid',
              gridTemplateColumns: 'minmax(180px, 1fr) auto auto auto',
              gap: 10,
              alignItems: 'center',
            }}>
              <Input
                value={lookupUsername}
                onChange={(value) => {
                  setLookupUsername(clampPlayerNameInput(value));
                  setMessage(null);
                }}
                onKeyDown={(event) => {
                  if (event.key === 'Enter' && canSaveLookupSkin) void saveUsernameSkin(false);
                }}
                placeholder="Minecraft username"
                icon="search"
              />
              <Pill tone={lookupUsernameError ? 'warn' : 'neutral'} icon={lookupUsernameError ? 'alert' : 'user'}>
                Player skin
              </Pill>
              <Button
                variant="secondary"
                size="sm"
                icon={lookupBusy ? 'refresh' : 'download'}
                disabled={!canSaveLookupSkin}
                onClick={() => void saveUsernameSkin(false)}
                title={lookupUsernameError || undefined}
              >
                Save skin
              </Button>
              <Button
                variant="ghost"
                size="sm"
                icon={lookupBusy ? 'refresh' : 'check'}
                disabled={!canSaveLookupSkin || !onlineReady}
                onClick={() => void saveUsernameSkin(true)}
                title={onlineReady
                  ? lookupUsernameError || 'Save locally, then apply to the active Minecraft account'
                  : 'Online Minecraft account required'}
              >
                Save & apply
              </Button>
            </div>
          </div>
        </details>

        {message && (
          <div style={{
            color: message.tone === 'err' ? 'var(--err)' : theme.n.textDim,
            fontSize: 12,
            fontWeight: 500,
            lineHeight: 1.4,
          }}>
            {message.text}
          </div>
        )}

        {state === 'unavailable' && (
          <div style={{ color: 'var(--err)', fontSize: 12, fontWeight: 500 }}>
            {error || 'Saved skins are unavailable.'}
          </div>
        )}

        {selectedSkin && (
          <div style={{
            display: 'grid',
            gridTemplateColumns: 'auto minmax(0, 1fr) auto',
            gap: 18,
            alignItems: 'center',
            padding: '2px 0 14px',
            borderBottom: '1px solid var(--line)',
          }}>
            <div style={{ display: 'grid', gap: 8, justifyItems: 'center' }}>
              <SavedSkinBodyPreview
                skin={selectedSkin}
                side={previewSide}
                showOuterLayers={previewLayerMode === 'full'}
              />
              <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', justifyContent: 'center' }}>
                <div role="group" aria-label={`${selectedSkin.name} body preview side`}>
                  <Segmented<SavedSkinPreviewSide>
                    options={[
                      { value: 'front', label: 'Front' },
                      { value: 'back', label: 'Back' },
                    ]}
                    value={previewSide}
                    onChange={setPreviewSide}
                  />
                </div>
                <div role="group" aria-label={`${selectedSkin.name} body preview layers`}>
                  <Segmented<SavedSkinLayerMode>
                    options={[
                      { value: 'full', label: 'Full' },
                      { value: 'base', label: 'Base' },
                    ]}
                    value={previewLayerMode}
                    onChange={setPreviewLayerMode}
                  />
                </div>
              </div>
            </div>
            <div style={{ minWidth: 0, display: 'grid', gap: 8 }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
                <Pill tone={selectedSkin.applied_at ? 'ok' : 'info'} icon={selectedSkin.applied_at ? 'check-circle' : 'image'}>
                  {selectedSkin.applied_at ? 'Equipped' : 'Previewing'}
                </Pill>
                <Pill tone="neutral" icon="tag">{savedSkinSourceLabel(selectedSkin.source)}</Pill>
                {!selectedPreviewEditing && <Pill tone="neutral">{selectedSkin.variant}</Pill>}
              </div>
              {selectedPreviewEditing ? (
                skinMetadataEditor(selectedSkin, {
                  display: 'grid',
                  gridTemplateColumns: 'minmax(180px, 360px) auto auto auto auto',
                  gap: 8,
                  alignItems: 'center',
                  minWidth: 0,
                  maxWidth: 720,
                })
              ) : (
                <div style={{
                  color: theme.n.text,
                  fontSize: 18,
                  fontWeight: 700,
                  lineHeight: 1.15,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                }}>
                  {selectedSkin.name}
                </div>
              )}
              <div style={{
                color: theme.n.textMute,
                fontSize: 12,
                fontFamily: theme.font.sans,
                overflow: 'hidden',
                textOverflow: 'ellipsis',
                whiteSpace: 'nowrap',
              }}>
                {selectedSkin.texture_key.slice(0, 16)} / {formatByteSize(selectedSkin.byte_size)}
              </div>
              <div style={{ color: theme.n.textDim, fontSize: 12.5, lineHeight: 1.45, maxWidth: 560 }}>
                Preview selection is local. Applying changes the active Minecraft profile only after the online upload succeeds.
              </div>
            </div>
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'flex-end', gap: 8, flexWrap: 'wrap' }}>
              {previewPending && (
                <Button
                  variant="ghost"
                  size="sm"
                  icon="refresh"
                  disabled={applyKey === selectedSkin.texture_key}
                  onClick={resetPreview}
                  title="Return preview to the equipped skin"
                >
                  Reset
                </Button>
              )}
              <Button
                variant="ghost"
                size="sm"
                icon="download"
                disabled={deleteKey === selectedSkin.texture_key}
                onClick={() => downloadSavedSkin(selectedSkin)}
                title="Download the normalized saved skin PNG"
              >
                Download PNG
              </Button>
              <Button
                variant={selectedSkin.applied_at ? 'ghost' : 'secondary'}
                size="sm"
                icon={selectedSkin.applied_at ? 'check-circle' : applyKey === selectedSkin.texture_key ? 'refresh' : 'check'}
                disabled={!onlineReady || Boolean(selectedSkin.applied_at) || applyKey === selectedSkin.texture_key}
                onClick={() => void applySkin(selectedSkin.texture_key)}
                title={selectedSkin.applied_at
                  ? 'Already applied to the active Minecraft account'
                  : onlineReady ? 'Apply to active Minecraft account' : 'Online Minecraft account required'}
              >
                {selectedSkin.applied_at ? 'Applied' : 'Apply'}
              </Button>
              <Button
                variant="ghost"
                size="sm"
                icon="edit"
                disabled={selectedPreviewEditing || deleteKey === selectedSkin.texture_key || applyKey === selectedSkin.texture_key}
                onClick={() => startEdit(selectedSkin)}
              >
                Edit
              </Button>
              <Button
                variant="ghost"
                size="sm"
                icon="trash"
                disabled={deleteKey === selectedSkin.texture_key || applyKey === selectedSkin.texture_key}
                onClick={() => void confirmDeleteSkin(selectedSkin)}
              >
                Delete
              </Button>
            </div>
          </div>
        )}

        <div style={{
          border: '1px solid var(--line)',
          borderRadius: theme.r.md,
          overflow: 'hidden',
          background: theme.n.surface2,
        }}>
          {state === 'loading' ? (
            <div style={{ padding: 14, color: theme.n.textMute, fontSize: 13, fontWeight: 500 }}>
              Loading saved skins...
            </div>
          ) : skins.length === 0 ? (
            <div style={{ padding: 14, color: theme.n.textMute, fontSize: 13, fontWeight: 500 }}>
              No saved skins yet.
            </div>
          ) : visibleSkins.length === 0 ? (
            <div style={{
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'space-between',
              gap: 10,
              flexWrap: 'wrap',
              padding: 14,
              color: theme.n.textMute,
              fontSize: 13,
              fontWeight: 500,
            }}>
              <span>No saved skins match this filter.</span>
              {normalizedSkinFilter && (
                <Button
                  variant="ghost"
                  size="sm"
                  icon="x"
                  onClick={() => setSkinFilter('')}
                >
                  Clear
                </Button>
              )}
            </div>
          ) : (
            visibleSkins.map((skin, index) => {
              const applied = Boolean(skin.applied_at);
              const selected = selectedSkin?.texture_key === skin.texture_key;
              const editing = editKey === skin.texture_key && !(selected && selectedPreviewEditing);
              const applying = applyKey === skin.texture_key;
              const deleting = deleteKey === skin.texture_key;
              const rowMenuItems = menuItemsForSavedSkin({
                skin,
                selectedPreviewEditing: selected && selectedPreviewEditing,
                onlineReady,
                applying,
                deleting,
                onView: () => setSelectedKey(skin.texture_key),
                onApply: () => void applySkin(skin.texture_key),
                onEdit: () => startEdit(skin),
                onDownload: () => downloadSavedSkin(skin),
                onDelete: () => void confirmDeleteSkin(skin),
              });

              return (
                <div
                  key={skin.texture_key}
                  style={{
                    display: 'grid',
                    gridTemplateColumns: editing ? '44px minmax(0, 1fr)' : '44px minmax(0, 1fr) auto auto',
                    gap: 12,
                    alignItems: 'center',
                    padding: '10px 12px',
                    background: selected ? 'var(--accent-softer)' : undefined,
                    borderTop: index === 0 ? undefined : '1px solid var(--line)',
                  }}
                  onContextMenu={editing || rowMenuItems.length === 0
                    ? undefined
                    : (event) => openContextMenu(event, rowMenuItems)}
                >
                  <SavedSkinBodyPreview
                    skin={skin}
                    side="front"
                    showOuterLayers
                    scale={2}
                    compact
                  />
                  {editing ? (
                    skinMetadataEditor(skin, {
                      display: 'grid',
                      gridTemplateColumns: 'minmax(180px, 1fr) auto auto auto auto',
                      gap: 8,
                      alignItems: 'center',
                      minWidth: 0,
                    })
                  ) : (
                    <>
                      <div style={{ minWidth: 0 }}>
                        <div style={{
                          color: theme.n.text,
                          fontSize: 13,
                          fontWeight: 700,
                          lineHeight: 1.25,
                          overflow: 'hidden',
                          textOverflow: 'ellipsis',
                          whiteSpace: 'nowrap',
                        }}>
                          {skin.name}
                        </div>
                        <div style={{
                          color: theme.n.textMute,
                          fontSize: 11,
                          fontWeight: 500,
                          lineHeight: 1.35,
                          fontFamily: theme.font.sans,
                          overflow: 'hidden',
                          textOverflow: 'ellipsis',
                          whiteSpace: 'nowrap',
                        }}>
                          {skin.texture_key.slice(0, 12)} / {formatByteSize(skin.byte_size)}
                        </div>
                      </div>
                      <div style={{
                        display: 'flex',
                        alignItems: 'center',
                        gap: 6,
                        flexWrap: 'wrap',
                        justifyContent: 'flex-end',
                      }}>
                        <Pill tone="neutral">{skin.variant}</Pill>
                        <Pill tone="neutral" icon="tag">{savedSkinSourceLabel(skin.source)}</Pill>
                        {applied && <Pill tone="ok" icon="check-circle">Equipped</Pill>}
                      </div>
                      <div style={{
                        display: 'flex',
                        alignItems: 'center',
                        justifyContent: 'flex-end',
                        gap: 8,
                        flexWrap: 'wrap',
                      }}>
                        <Button
                          variant={selected ? 'secondary' : 'ghost'}
                          size="sm"
                          icon="image"
                          disabled={deleting}
                          onClick={() => setSelectedKey(skin.texture_key)}
                        >
                          View
                        </Button>
                        <Button
                          variant={applied ? 'ghost' : 'secondary'}
                          size="sm"
                          icon={applied
                            ? 'check-circle'
                            : applying ? 'refresh' : 'check'}
                          disabled={!onlineReady || applied || applying}
                          onClick={() => void applySkin(skin.texture_key)}
                          title={applied
                            ? 'Already applied to the active Minecraft account'
                            : onlineReady ? 'Apply to active Minecraft account' : 'Online Minecraft account required'}
                        >
                          {applied ? 'Applied' : 'Apply'}
                        </Button>
                        <IconButton
                          icon="dots"
                          size={28}
                          tooltip="More skin actions"
                          disabled={rowMenuItems.length === 0}
                          onClick={(event) => {
                            event.stopPropagation();
                            openContextMenu(event, rowMenuItems);
                          }}
                        />
                      </div>
                    </>
                  )}
                </div>
              );
            })
          )}
        </div>
      </div>
    </Card>
  );
}

function readApiPayloadMessage(payload: unknown, fallback: string): string {
  if (isRecord(payload) && typeof payload.error === 'string' && payload.error.trim()) {
    return payload.error.trim();
  }
  return fallback;
}

function savedSkinApplyErrorMessage(error: unknown): string {
  return `Skin saved locally, but could not apply to Minecraft profile: ${
    boundedMessage(error instanceof Error ? error.message : undefined, 'Minecraft profile apply failed.')
  }`;
}

function formatByteSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '0 B';
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  return `${(bytes / 1024).toFixed(1)} KiB`;
}

function savedSkinFileUrl(skin: SavedSkinRecord): string {
  return apiResourceUrl(`/skins/${skin.texture_key}/file`);
}

function savedSkinDownloadFilename(skin: SavedSkinRecord): string {
  const name = skin.name
    .trim()
    .replace(/[\\/:*?"<>|\x00-\x1f]+/g, ' ')
    .replace(/\s+/g, ' ')
    .slice(0, 80)
    .trim();
  return `${name || 'saved-skin'}.png`;
}

function SkinPreviewPart({
  src,
  x,
  y,
  w,
  h,
  scale,
  style,
}: {
  src: string;
  x: number;
  y: number;
  w: number;
  h: number;
  scale: number;
  style?: JSX.CSSProperties;
}): JSX.Element {
  return (
    <span
      aria-hidden="true"
      style={{
        display: 'block',
        width: w * scale,
        height: h * scale,
        backgroundImage: `url("${src}")`,
        backgroundRepeat: 'no-repeat',
        backgroundSize: `${64 * scale}px ${64 * scale}px`,
        backgroundPosition: `-${x * scale}px -${y * scale}px`,
        imageRendering: 'pixelated',
        ...style,
      }}
    />
  );
}

function SkinBodyPreview({
  src,
  name,
  variant,
  side,
  showOuterLayers,
  scale = 6,
  compact = false,
}: {
  src: string;
  name: string;
  variant: SkinVariant;
  side: SavedSkinPreviewSide;
  showOuterLayers: boolean;
  scale?: number;
  compact?: boolean;
}): JSX.Element {
  const slim = variant === 'slim';
  const armWidth = slim ? 3 : 4;
  const frameStyle: JSX.CSSProperties = compact
    ? {
        width: 40,
        minHeight: 64,
        padding: 0,
        border: '0',
        borderRadius: 0,
        background: 'transparent',
        boxShadow: 'none',
      }
    : {
        width: 118,
        minHeight: 208,
        padding: 12,
        border: '1px solid var(--line)',
        borderRadius: 'var(--r-md)',
        background: 'var(--surface-2)',
        boxShadow: 'inset 0 1px 0 color-mix(in oklab, var(--text) 5%, transparent)',
      };
  const parts = side === 'front'
    ? {
      head: { x: 8, y: 8 },
      headOverlay: { x: 40, y: 8 },
      rightArm: { x: 44, y: 20, w: armWidth },
      rightArmOverlay: { x: 44, y: 36 },
      body: { x: 20, y: 20 },
      bodyOverlay: { x: 20, y: 36 },
      leftArm: { x: 36, y: 52, w: armWidth },
      leftArmOverlay: { x: 52, y: 52 },
      rightLeg: { x: 4, y: 20 },
      rightLegOverlay: { x: 4, y: 36 },
      leftLeg: { x: 20, y: 52 },
      leftLegOverlay: { x: 4, y: 52 },
    }
    : {
      head: { x: 24, y: 8 },
      headOverlay: { x: 56, y: 8 },
      rightArm: { x: slim ? 51 : 52, y: 20, w: armWidth },
      rightArmOverlay: { x: slim ? 51 : 52, y: 36 },
      body: { x: 32, y: 20 },
      bodyOverlay: { x: 32, y: 36 },
      leftArm: { x: slim ? 43 : 44, y: 52, w: armWidth },
      leftArmOverlay: { x: slim ? 59 : 60, y: 52 },
      rightLeg: { x: 12, y: 20 },
      rightLegOverlay: { x: 12, y: 36 },
      leftLeg: { x: 28, y: 52 },
      leftLegOverlay: { x: 12, y: 52 },
    };
  const previewPart = (
    part: { x: number; y: number },
    overlay: { x: number; y: number },
    w: number,
    h: number,
  ): JSX.Element => (
    <span style={{ display: 'block', position: 'relative', width: w * scale, height: h * scale }}>
      <SkinPreviewPart src={src} x={part.x} y={part.y} w={w} h={h} scale={scale} />
      {showOuterLayers && (
        <SkinPreviewPart
          src={src}
          x={overlay.x}
          y={overlay.y}
          w={w}
          h={h}
          scale={scale}
          style={{ position: 'absolute', inset: 0 }}
        />
      )}
    </span>
  );

  return (
    <div
      role="img"
      aria-label={`${name} ${side} ${showOuterLayers ? 'full' : 'base'} skin preview`}
      style={{
        ...frameStyle,
        display: 'grid',
        alignContent: 'center',
        justifyItems: 'center',
        gap: 0,
      }}
    >
      <div style={{ position: 'relative', width: 8 * scale, height: 8 * scale, marginBottom: 2 }}>
        <SkinPreviewPart src={src} x={parts.head.x} y={parts.head.y} w={8} h={8} scale={scale} />
        {showOuterLayers && (
          <SkinPreviewPart
            src={src}
            x={parts.headOverlay.x}
            y={parts.headOverlay.y}
            w={8}
            h={8}
            scale={scale}
            style={{ position: 'absolute', inset: 0 }}
          />
        )}
      </div>
      <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'center' }}>
        {previewPart(parts.rightArm, parts.rightArmOverlay, parts.rightArm.w, 12)}
        {previewPart(parts.body, parts.bodyOverlay, 8, 12)}
        {previewPart(parts.leftArm, parts.leftArmOverlay, parts.leftArm.w, 12)}
      </div>
      <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'center' }}>
        {previewPart(parts.rightLeg, parts.rightLegOverlay, 4, 12)}
        {previewPart(parts.leftLeg, parts.leftLegOverlay, 4, 12)}
      </div>
    </div>
  );
}

function SavedSkinBodyPreview({
  skin,
  side,
  showOuterLayers,
  scale,
  compact,
}: {
  skin: SavedSkinRecord;
  side: SavedSkinPreviewSide;
  showOuterLayers: boolean;
  scale?: number;
  compact?: boolean;
}): JSX.Element {
  return (
    <SkinBodyPreview
      src={savedSkinFileUrl(skin)}
      name={skin.name}
      variant={skin.variant}
      side={side}
      showOuterLayers={showOuterLayers}
      scale={scale}
      compact={compact}
    />
  );
}

export function AccountsView(): JSX.Element {
  const cfg = config.value;
  const savedUsername = cfg?.username || 'Player';
  const { status, state, refresh } = useAuthStatus(savedUsername);
  const onlineReady = state === 'ready' && Boolean(status?.online_mode_ready);

  return (
    <div class="cp-view-page" style={{ gap: 20 }}>
      <div class="cp-page-header">
        <div>
          <h1>Accounts & skins</h1>
          <div class="cp-page-sub">Offline identity, account boundaries, and server skin commands.</div>
        </div>
      </div>

      <PlayerIdentityCard savedUsername={savedUsername} />

      <AccountBoundary savedUsername={savedUsername} />

      <SavedSkinLibrary
        onlineReady={onlineReady}
        minecraftProfile={status?.minecraft_profile}
        onApplied={refresh}
      />

      <SkinRestorerHelper savedUsername={savedUsername} />
    </div>
  );
}
