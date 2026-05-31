import type { JSX } from 'preact';
import { useCallback, useEffect, useState } from 'preact/hooks';
import { api, apiResourceUrl } from '../../api';
import { Button, Card, Input, Pill, SectionHeading } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { PlayerHeadPreview } from '../../ui/PlayerHeadPreview';
import { useTheme } from '../../hooks/use-theme';
import { clampPlayerNameInput, savePlayerName } from '../../player-name';
import { config } from '../../store';
import type { LaunchAuthMode } from '../../types';
import { validateUsername } from '../../utils';

interface OfflineSkinProfile {
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

function PlayerIdentityEditor({
  savedUsername,
  headSrc,
}: {
  savedUsername: string;
  headSrc?: string;
}): JSX.Element {
  const theme = useTheme();
  const [username, setUsername] = useState(savedUsername);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const nameError = validateUsername(username);
  const nameValid = nameError === null;
  const showNameError = username.length > 0 && !nameValid;
  const previewSrc = username.trim() === savedUsername ? headSrc : undefined;

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
        size={96}
        radius={theme.r.md}
        ariaLabel={`Offline skin preview for ${username.trim() || 'Player'}`}
        title="Offline skin preview"
      />
      <div style={{ flex: 1, minWidth: 240 }}>
        <div style={{
          fontSize: 11, fontWeight: 600, color: theme.n.textMute,
          textTransform: 'uppercase', letterSpacing: 0.8, marginBottom: 6,
        }}>Player name</div>
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
        letterSpacing: 0.7,
      }}>{label}</div>
      <div style={{
        color: theme.n.textDim,
        fontFamily: label === 'UUID' ? theme.font.mono : undefined,
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
  profile: OfflineSkinProfile | null;
  state: OfflineProfileState;
} {
  const [profile, setProfile] = useState<OfflineSkinProfile | null>(null);
  const [state, setState] = useState<OfflineProfileState>('loading');

  useEffect(() => {
    let active = true;
    setState('loading');
    setProfile(null);

    void api('GET', '/skin/profile')
      .then((res: OfflineSkinProfile & { error?: string }) => {
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

function OfflineProfileMeta({
  profile,
  state,
}: {
  profile: OfflineSkinProfile | null;
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
        Offline profile
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
          <ProfileMetaValue label="Source" value={profile.source || 'default'} />
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
  const headSrc = state === 'ready' && profile?.username === savedUsername && profile.head_url
    ? apiResourceUrl(profile.head_url)
    : undefined;

  return (
    <Card>
      <SectionHeading eyebrow="Player" title="Identity" />
      <PlayerIdentityEditor key={savedUsername} savedUsername={savedUsername} headSrc={headSrc} />
      <OfflineProfileMeta profile={profile} state={state} />
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
      text: 'Online is selected and the backend reports launch credentials ready. They are volatile: Croopor does not persist or refresh them yet.',
    };
  }

  return {
    tone: 'warn',
    icon: 'alert',
    text: 'Online is selected, but launch credentials are not ready. Sign in and verify Minecraft ownership again, or switch to Offline for the reliable path.',
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
          ? 'Online launch mode saved. Credentials are volatile and are not persisted or refreshed yet.'
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
            letterSpacing: 0.7,
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
            ? 'Uses the verified Minecraft profile while these volatile credentials are valid.'
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
    return `${profileName || 'Minecraft profile'} was verified. Online launch mode can be selected while these volatile credentials remain valid.`;
  }
  if (poll.minecraft_profile_ready) {
    return `${profileName || 'Minecraft profile'} was found, but ownership was not verified. Offline launch mode remains available.`;
  }
  return 'Microsoft sign-in is active, but Minecraft profile verification is not complete. Offline launch mode remains available.';
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
    ? formatSeconds(status.minecraft_token_expires_in)
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
        Online launch mode can use this profile only while Online is selected and the volatile credentials remain valid. Croopor does not persist or refresh them yet.
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
            letterSpacing: 0.7,
          }}>Microsoft code</div>
          <div style={{
            color: theme.n.text,
            fontFamily: theme.font.mono,
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
        .catch(() => {
          if (!active) return;
          setLogin(null);
          setPollHint(null);
          setLoginError('Could not reach the local backend while polling Microsoft sign-in.');
        });
    }, Math.max(1, login.interval) * 1000);

    return () => {
      active = false;
      window.clearTimeout(timeout);
    };
  }, [login, refreshStatus]);

  const startLogin = async (): Promise<void> => {
    if (loginBusy) return;
    setLoginBusy(true);
    setLogin(null);
    setLoginError(null);
    setLogoutError(null);
    setLoginSuccess(null);
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
    } catch {
      setLogin(null);
      setLoginError('Could not reach the local backend.');
    } finally {
      setLoginBusy(false);
    }
  };

  const logout = async (): Promise<void> => {
    if (logoutBusy) return;
    setLogoutBusy(true);
    setLogin(null);
    setPollHint(null);
    setLoginError(null);
    setLogoutError(null);
    setLoginSuccess(null);
    try {
      const response = await api('POST', '/auth/logout');
      if (typeof response === 'object' && response !== null && typeof response.error === 'string') {
        setLogoutError(logoutErrorMessage(response));
      } else {
        setLoginSuccess('Microsoft sign-in was cleared. Switch to Offline for the reliable launch path if Online was selected.');
      }
    } catch {
      setLogoutError('Could not reach the local backend to clear Microsoft sign-in.');
    } finally {
      refreshStatus();
      setLogoutBusy(false);
    }
  };

  const msaActive = Boolean(status?.msa_authenticated);
  const minecraftVerified = Boolean(status?.minecraft_profile_ready === true && status?.minecraft_ownership_verified === true);
  const minecraftReadinessReported = status ? hasMinecraftReadiness(status) : false;
  const effectiveModeLabel = status?.mode === 'online' ? 'online' : 'offline';
  const statusCopy = msaActive
    ? minecraftVerified
      ? `Microsoft sign-in is active and the Minecraft profile is verified. Croopor launches as ${status?.username} when Online is selected and the volatile credentials remain valid.`
      : minecraftReadinessReported
        ? `Microsoft sign-in is active, but verified Minecraft profile ownership is not ready. Offline launch remains available.`
        : `Microsoft sign-in is active. Online launch still needs Minecraft profile and ownership readiness; offline launch remains available.`
    : `Croopor is using ${status?.username} as the current ${effectiveModeLabel} identity. Online launch credentials are ${status?.online_mode_ready ? 'reported ready by the backend' : 'not ready'}.`;

  return (
    <Card>
      <SectionHeading
        eyebrow="Account"
        title="Minecraft account"
        right={(
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
            <Pill tone={statusTone} icon="user">{statusLabel}</Pill>
            {msaActive && <Pill tone="ok" icon="check-circle">Microsoft active</Pill>}
            {minecraftVerified && <Pill tone="ok" icon="shield-check">Minecraft verified</Pill>}
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
            <div style={{
              display: 'grid',
              gridTemplateColumns: 'repeat(auto-fit, minmax(124px, 1fr))',
              gap: 12,
              alignItems: 'start',
            }}>
              <ProfileMetaValue label="Identity" value={status.mode === 'online' ? 'Online profile' : 'Offline profile'} />
              <ProfileMetaValue label="Verified" value={status.verified ? 'Yes' : 'No'} />
              <ProfileMetaValue label="UUID" value={shortenUuid(status.uuid)} />
              <ProfileMetaValue label="Skin" value={status.skin_source || 'default'} />
              <ProfileMetaValue label="Login" value={status.login_available ? 'Available' : 'Unavailable'} />
              <ProfileMetaValue label="Microsoft" value={msaActive ? 'Active' : 'Inactive'} />
            </div>
            <MinecraftProfileReadiness status={status} />
            <div style={{
              display: 'flex',
              alignItems: 'center',
              gap: 10,
              flexWrap: 'wrap',
            }}>
              {msaActive ? (
                <Button
                  variant="secondary"
                  icon="x"
                  onClick={() => void logout()}
                  disabled={logoutBusy}
                  sound="affirm"
                >
                  {logoutBusy ? 'Signing out' : 'Log out'}
                </Button>
              ) : status.login_available ? (
                <Button
                  variant="secondary"
                  icon="globe"
                  onClick={() => void startLogin()}
                  disabled={loginBusy}
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
                {msaActive
                  ? 'Logout clears only volatile Microsoft state. Offline identity and launches remain available.'
                  : 'Microsoft sign-in prepares Online launch mode. It does not switch launch mode by itself.'}
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
            {state === 'ready' && status
              ? `${status.login_reason}. Offline launches remain available for singleplayer and offline-mode servers.`
              : 'Microsoft sign-in status will appear here when the backend is reachable.'}
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
        eyebrow="Skins"
        title="Server skin helper"
        right={<Pill tone="neutral" icon="terminal">SkinRestorer</Pill>}
      />
      <div style={{ display: 'grid', gap: 14 }}>
        <div style={{ fontSize: 13, color: theme.n.textDim, lineHeight: 1.5, maxWidth: 760 }}>
          For servers that use SkinRestorer, copy a command that points your server skin at a Minecraft username. This is a server-side command helper only; Croopor does not upload skins or contact skin services from this page.
        </div>

        <div style={{
          display: 'grid',
          gridTemplateColumns: 'minmax(220px, 360px) minmax(220px, 1fr) auto',
          gap: 10,
          alignItems: 'end',
        }}>
          <label style={{ display: 'grid', gap: 6, minWidth: 0 }}>
            <span style={{
              fontSize: 11,
              fontWeight: 600,
              color: theme.n.textMute,
              textTransform: 'uppercase',
              letterSpacing: 0.8,
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

          <div style={{ display: 'grid', gap: 6, minWidth: 0 }}>
            <div style={{
              fontSize: 11,
              fontWeight: 600,
              color: theme.n.textMute,
              textTransform: 'uppercase',
              letterSpacing: 0.8,
            }}>Command</div>
            <div
              aria-label="SkinRestorer command"
              style={{
                minHeight: 38,
                display: 'flex',
                alignItems: 'center',
                padding: '0 12px',
                border: '1px solid var(--line)',
                borderRadius: theme.r.md,
                background: theme.n.surface2,
                color: canCopy ? theme.n.text : theme.n.textMute,
                fontFamily: theme.font.mono,
                fontSize: 12,
                overflow: 'hidden',
                whiteSpace: 'nowrap',
                textOverflow: 'ellipsis',
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
    </Card>
  );
}

export function AccountsView(): JSX.Element {
  const cfg = config.value;
  const savedUsername = cfg?.username || 'Player';

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

      <SkinRestorerHelper savedUsername={savedUsername} />
    </div>
  );
}
