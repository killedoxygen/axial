import type { JSX } from 'preact';
import { useCallback, useEffect, useState } from 'preact/hooks';
import { api, apiResourceUrl } from '../../api';
import { Button, Card, Input, Pill, SectionHeading } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { PlayerHeadPreview } from '../../ui/PlayerHeadPreview';
import { useTheme } from '../../hooks/use-theme';
import { clampPlayerNameInput, savePlayerName } from '../../player-name';
import { config } from '../../store';
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

interface AuthStatus {
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

type AuthPollTerminalStatus = 'authorization_declined' | 'expired' | 'bad_verification_code' | 'error';

interface AuthPollTerminal {
  status: AuthPollTerminalStatus;
  error?: string;
  poll_hint?: string;
}

type AuthPollResponse = AuthPollPending | AuthPollAuthenticated | AuthPollTerminal;

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
  status: AuthStatus | null;
  state: AuthStatusState;
  refresh: () => void;
} {
  const [status, setStatus] = useState<AuthStatus | null>(null);
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
      .then((res: AuthStatus & { error?: string }) => {
        if (!active) return;
        if (res.error) throw new Error(res.error);
        setStatus(res);
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

function loginPendingResponse(value: unknown): AuthLoginPending | null {
  if (typeof value !== 'object' || value === null) return null;
  const record = value as Record<string, unknown>;
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
  if (typeof value !== 'object' || value === null) {
    return fallback;
  }
  const record = value as Record<string, unknown>;
  return boundedMessage(typeof record.error === 'string' ? record.error : undefined, fallback);
}

function loginErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not start Microsoft sign-in.');
}

function logoutErrorMessage(value: unknown): string {
  return apiErrorMessage(value, 'Could not clear Microsoft sign-in.');
}

function pollResponse(value: unknown): AuthPollResponse | null {
  if (typeof value !== 'object' || value === null) return null;
  const record = value as Record<string, unknown>;
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
      msa_token_expires_in: typeof record.msa_token_expires_in === 'number'
        ? record.msa_token_expires_in
        : undefined,
    };
  }

  if (
    record.status === 'authorization_declined' ||
    record.status === 'expired' ||
    record.status === 'bad_verification_code' ||
    record.status === 'error'
  ) {
    return {
      status: record.status,
      error: typeof record.error === 'string' ? record.error : undefined,
      poll_hint: typeof record.poll_hint === 'string' ? record.poll_hint : undefined,
    };
  }

  return null;
}

function pollTerminalMessage(response: AuthPollTerminal | null): string {
  if (!response) return 'Microsoft sign-in returned an unexpected response.';
  const fallback = response.status === 'authorization_declined'
    ? 'Microsoft sign-in was declined.'
    : response.status === 'expired'
      ? 'Microsoft sign-in expired. Get a new code to try again.'
      : response.status === 'bad_verification_code'
        ? 'Microsoft sign-in code was rejected. Get a new code to try again.'
        : 'Microsoft sign-in could not be completed.';
  return boundedMessage(response.error || response.poll_hint, fallback);
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
          {' '}This starts sign-in only. Online mode is not ready until the future token flow is added.
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
            setLoginSuccess('Microsoft sign-in is active for this launcher session. Launch identity remains offline and unverified.');
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
        setLoginSuccess('Microsoft sign-in was cleared. Offline launches are unchanged.');
      }
    } catch {
      setLogoutError('Could not reach the local backend to clear Microsoft sign-in.');
    } finally {
      refreshStatus();
      setLogoutBusy(false);
    }
  };

  const msaActive = Boolean(status?.msa_authenticated);
  const msaProvider = status?.msa_provider || 'Microsoft';
  const statusCopy = msaActive
    ? `${msaProvider} sign-in is active. Croopor still launches as ${status?.username}, an offline unverified identity, until the future Minecraft profile chain exists.`
    : `Croopor is using ${status?.username} as the current ${status?.provider} identity. Online-mode sessions are ${status?.online_mode_ready ? 'ready' : 'not ready'}.`;

  return (
    <Card>
      <SectionHeading
        eyebrow="Account"
        title="Minecraft account"
        right={(
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
            <Pill tone={statusTone} icon="user">{statusLabel}</Pill>
            {msaActive && <Pill tone="ok" icon="check-circle">Microsoft active</Pill>}
          </div>
        )}
      />
      <div style={{ display: 'grid', gap: 12 }}>
        {state === 'ready' && status ? (
          <>
            <div style={{ fontSize: 13, color: theme.n.textDim, lineHeight: 1.5, maxWidth: 780 }}>
              {statusCopy}
            </div>
            <div style={{
              display: 'grid',
              gridTemplateColumns: 'repeat(auto-fit, minmax(124px, 1fr))',
              gap: 12,
              alignItems: 'start',
            }}>
              <ProfileMetaValue label="Provider" value={status.provider} />
              <ProfileMetaValue label="Verified" value={status.verified ? 'Yes' : 'No'} />
              <ProfileMetaValue label="UUID" value={shortenUuid(status.uuid)} />
              <ProfileMetaValue label="Skin" value={status.skin_source || 'default'} />
              <ProfileMetaValue label="Login" value={status.login_available ? 'Available' : 'Unavailable'} />
              <ProfileMetaValue label="Microsoft" value={msaActive ? msaProvider : 'Inactive'} />
            </div>
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
                  : 'Microsoft sign-in is a setup step. It does not switch this launcher to online mode yet.'}
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
