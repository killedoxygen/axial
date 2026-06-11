import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { api, apiResourceUrl, isApiError } from '../../api';
import { Button, IconButton } from '../../ui/Atoms';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { showConfirm } from '../../ui/Dialog';
import { Icon } from '../../ui/Icons';
import { Modal, ModalContent, ModalHeader, ModalTitle } from '../../ui/Modal';
import { PlayerHeadPreview } from '../../ui/PlayerHeadPreview';
import { promptPlayerName, savePlayerName } from '../../player-name';
import { selectedSkinTextureSrc } from '../../player-skin';
import { config } from '../../store';
import { localStateVersion } from '../../state';
import type { LaunchAuthMode } from '../../types';
import { isRecord } from './api';
import {
  configErrorMessage,
  copyText,
  formatSeconds,
  launchAuthMode,
  loginErrorMessage,
  loginPendingResponse,
  logoutErrorMessage,
  pollErrorMessage,
  pollResponse,
  pollSuccessMessage,
  pollTerminalMessage,
  statusCanSelectOnline,
  authRefreshErrorMessage,
  type AuthLoginPending,
} from './auth';
import type { AuthStatusRecord, AuthStatusState } from './types';

type Message = { tone: 'ok' | 'err'; text: string } | null;

function IdentityRow({
  head,
  name,
  detail,
  active,
  disabled,
  selectTitle,
  onSelect,
  menuItems,
}: {
  head: JSX.Element;
  name: string;
  detail: string;
  active: boolean;
  disabled?: boolean;
  selectTitle?: string;
  onSelect?: () => void;
  menuItems?: ContextMenuItem[];
}): JSX.Element {
  return (
    <div class="cp-account-rowwrap">
      <button
        type="button"
        class="cp-account-row"
        data-active={active ? 'true' : 'false'}
        disabled={disabled}
        title={selectTitle}
        onClick={onSelect}
        onContextMenu={menuItems && menuItems.length > 0
          ? (event) => {
              event.preventDefault();
              openContextMenu(event, menuItems);
            }
          : undefined}
      >
        {head}
        <span class="cp-account-row__id">
          <span class="cp-account-row__name">{name}</span>
          <span class="cp-account-row__detail">{detail}</span>
        </span>
        <span class="cp-account-row__state">
          {active && <Icon name="check-circle" size={16} />}
        </span>
      </button>
      {menuItems && menuItems.length > 0 && (
        <span class="cp-account-rowwrap__menu">
          <IconButton
            icon="dots"
            size={26}
            tooltip="Account actions"
            onClick={(event) => {
              event.stopPropagation();
              openContextMenu(event, menuItems);
            }}
          />
        </span>
      )}
    </div>
  );
}

function DeviceCodePanel({
  login,
  pollHint,
  onCancel,
}: {
  login: AuthLoginPending;
  pollHint: string | null;
  onCancel: () => void;
}): JSX.Element {
  const [copied, setCopied] = useState<'code' | 'url' | null>(null);

  const copy = (target: 'code' | 'url', value: string): void => {
    void copyText(value)
      .then(() => setCopied(target))
      .catch(() => setCopied(null));
  };

  return (
    <div class="cp-account-devicecode">
      <div class="cp-account-devicecode__row">
        <span class="cp-account-devicecode__code">{login.user_code}</span>
        <Button
          variant="secondary"
          size="sm"
          icon={copied === 'code' ? 'check' : 'copy'}
          onClick={() => copy('code', login.user_code)}
          sound="affirm"
        >
          {copied === 'code' ? 'Copied' : 'Copy code'}
        </Button>
        <Button variant="ghost" size="sm" icon="x" onClick={onCancel}>
          Cancel
        </Button>
      </div>
      <div class="cp-account-devicecode__row">
        <a href={login.verification_uri} target="_blank" rel="noreferrer">
          {login.verification_uri}
        </a>
        <IconButton
          icon={copied === 'url' ? 'check' : 'copy'}
          size={26}
          tooltip="Copy verification URL"
          onClick={() => copy('url', login.verification_uri)}
        />
      </div>
      <div class="cp-account-devicecode__meta">
        <span>Enter the code at the Microsoft page.</span>
        <span>Expires in {formatSeconds(login.expires_in)}</span>
        <span>{pollHint || 'Waiting for approval'}</span>
      </div>
    </div>
  );
}

export function AccountSwitcher({
  status,
  state,
  savedUsername,
  onChanged,
}: {
  status: AuthStatusRecord | null;
  state: AuthStatusState;
  savedUsername: string;
  onChanged: () => void;
}): JSX.Element {
  const [open, setOpen] = useState(false);
  const [login, setLogin] = useState<AuthLoginPending | null>(null);
  const [loginBusy, setLoginBusy] = useState(false);
  const [pollHint, setPollHint] = useState<string | null>(null);
  const [logoutBusy, setLogoutBusy] = useState(false);
  const [refreshBusy, setRefreshBusy] = useState(false);
  const [modeBusy, setModeBusy] = useState<LaunchAuthMode | null>(null);
  const [message, setMessage] = useState<Message>(null);

  const msaActive = Boolean(status?.msa_authenticated);
  const profile = status?.minecraft_profile;
  const profileName = profile?.name ?? (msaActive ? status?.username ?? '' : '');
  const onlineSelectable = status ? statusCanSelectOnline(status) : false;
  const savedMode = launchAuthMode(config.value?.launch_auth_mode ?? status?.launch_auth_mode);
  const onlineActive = savedMode === 'online';
  const refreshAvailable = Boolean(status?.login_available && status?.msa_refresh_available);
  const profileTextureSrc = profile ? apiResourceUrl('/skin/profile/file') : undefined;
  localStateVersion.value;
  const accountTextureSrc = selectedSkinTextureSrc() ?? undefined;
  const busy = loginBusy || logoutBusy || refreshBusy || modeBusy !== null;
  const chipName = onlineActive && profileName ? profileName : savedUsername;

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
            setMessage({ tone: 'err', text: pollTerminalMessage(null) });
            return;
          }
          if (poll.status === 'pending') {
            setPollHint(poll.poll_hint ?? null);
            setLogin((current) => current?.login_id === login.login_id
              ? { ...current, interval: poll.interval }
              : current);
            return;
          }
          if (poll.status === 'msa_authenticated') {
            setLogin(null);
            setPollHint(null);
            setMessage({ tone: 'ok', text: pollSuccessMessage(poll) });
            onChanged();
            return;
          }
          setLogin(null);
          setPollHint(null);
          setMessage({ tone: 'err', text: pollTerminalMessage(poll) });
        })
        .catch((err: unknown) => {
          if (!active) return;
          setLogin(null);
          setPollHint(null);
          setMessage({
            tone: 'err',
            text: isApiError(err)
              ? pollErrorMessage(err.payload)
              : 'Could not reach the local backend while polling Microsoft sign-in.',
          });
        });
    }, Math.max(1, login.interval) * 1000);

    return () => {
      active = false;
      window.clearTimeout(timeout);
    };
  }, [login, onChanged]);

  const startLogin = async (): Promise<void> => {
    if (busy) return;
    setLoginBusy(true);
    setLogin(null);
    setMessage(null);
    setPollHint(null);
    try {
      const response = await api('POST', '/auth/login');
      const pending = loginPendingResponse(response);
      if (pending) {
        setLogin(pending);
        return;
      }
      setMessage({ tone: 'err', text: loginErrorMessage(response) });
    } catch (err: unknown) {
      setMessage({
        tone: 'err',
        text: isApiError(err) ? loginErrorMessage(err.payload) : 'Could not reach the local backend.',
      });
    } finally {
      setLoginBusy(false);
    }
  };

  const logout = async (): Promise<void> => {
    if (busy) return;
    const ok = await showConfirm(
      'Sign out of the Microsoft account? Offline launches stay available.',
      { title: 'Sign out', destructive: true, confirmText: 'Sign out' },
    );
    if (!ok) return;
    setLogoutBusy(true);
    setLogin(null);
    setPollHint(null);
    setMessage(null);
    try {
      const response = await api('POST', '/auth/logout');
      if (isRecord(response) && typeof response.error === 'string') {
        setMessage({ tone: 'err', text: logoutErrorMessage(response) });
      } else {
        setMessage({ tone: 'ok', text: 'Microsoft sign-in cleared.' });
      }
    } catch (err: unknown) {
      setMessage({
        tone: 'err',
        text: isApiError(err)
          ? logoutErrorMessage(err.payload)
          : 'Could not reach the local backend to clear Microsoft sign-in.',
      });
    } finally {
      onChanged();
      setLogoutBusy(false);
    }
  };

  const refreshAuth = async (): Promise<void> => {
    if (busy || login) return;
    setRefreshBusy(true);
    setMessage(null);
    try {
      const response = await api('POST', '/auth/refresh');
      if (isRecord(response) && typeof response.error === 'string') {
        setMessage({ tone: 'err', text: authRefreshErrorMessage(response) });
        return;
      }
      if (!isRecord(response) || response.status !== 'refreshed') {
        setMessage({ tone: 'err', text: 'Microsoft sign-in refresh returned an unexpected response.' });
        return;
      }
      setMessage({ tone: 'ok', text: 'Microsoft sign-in refreshed.' });
    } catch (err: unknown) {
      setMessage({
        tone: 'err',
        text: isApiError(err)
          ? authRefreshErrorMessage(err.payload)
          : 'Could not reach the local backend to refresh Microsoft sign-in.',
      });
    } finally {
      onChanged();
      setRefreshBusy(false);
    }
  };

  const useMode = async (nextMode: LaunchAuthMode): Promise<void> => {
    if (busy || nextMode === savedMode) return;
    if (nextMode === 'online' && !onlineSelectable) {
      setMessage({
        tone: 'err',
        text: 'Online needs a verified, Java-owning Minecraft account with valid credentials.',
      });
      return;
    }
    setModeBusy(nextMode);
    setMessage(null);
    try {
      const response = await api('PUT', '/config', { launch_auth_mode: nextMode });
      if (isRecord(response) && typeof response.error === 'string') {
        setMessage({ tone: 'err', text: configErrorMessage(response) });
        return;
      }
      config.value = response;
      onChanged();
    } catch {
      setMessage({ tone: 'err', text: 'Could not reach the local backend to save launch mode.' });
    } finally {
      setModeBusy(null);
    }
  };

  const renameOffline = async (): Promise<void> => {
    const next = await promptPlayerName(savedUsername);
    if (!next) return;
    const saved = await savePlayerName(next);
    if (saved) onChanged();
  };

  const microsoftMenuItems: ContextMenuItem[] = [
    ...(refreshAvailable
      ? [{ icon: 'refresh', label: 'Refresh credentials', onSelect: () => void refreshAuth() }]
      : []),
    ...(status?.login_available
      ? [{ icon: 'globe', label: 'Re-verify with device code', onSelect: () => void startLogin() }]
      : []),
    { label: '', onSelect: () => {}, divider: true },
    { icon: 'x', label: 'Sign out', onSelect: () => void logout(), danger: true },
  ];

  const offlineMenuItems: ContextMenuItem[] = [
    { icon: 'edit', label: 'Rename', onSelect: () => void renameOffline() },
  ];

  return (
    <>
      <button
        type="button"
        class="cp-account-chip"
        onClick={() => {
          setMessage(null);
          setOpen(true);
        }}
        title="Switch account or identity"
      >
        <PlayerHeadPreview
          username={chipName || 'Player'}
          textureSrc={accountTextureSrc}
          size={30}
          radius={7}
          ariaLabel={`${chipName || 'Player'} account`}
        />
        <span class="cp-account-chip__id">
          <span class="cp-account-chip__name">{state === 'loading' ? 'Loading' : chipName || 'Player'}</span>
          <span class="cp-account-chip__mode">{onlineActive ? 'Microsoft account' : 'Offline identity'}</span>
        </span>
        <Icon name="chevron-down" size={14} color="var(--text-dim)" />
      </button>

      <Modal open={open} onOpenChange={setOpen}>
        <ModalContent className="cp-account-modal" aria-label="Accounts" aria-describedby={undefined}>
          <ModalHeader>
            <ModalTitle>Accounts</ModalTitle>
          </ModalHeader>
          <div class="cp-account-rows">
            {msaActive || profile ? (
              <IdentityRow
                head={(
                  <PlayerHeadPreview
                    username={profileName || 'Player'}
                    textureSrc={onlineActive ? accountTextureSrc : profileTextureSrc}
                    size={38}
                    radius={9}
                    ariaLabel={`${profileName || 'Microsoft'} profile head`}
                  />
                )}
                name={profileName || 'Microsoft account'}
                detail={onlineSelectable ? 'Microsoft account' : 'Microsoft account, not ready for online launch'}
                active={onlineActive}
                disabled={busy || (!onlineActive && !onlineSelectable)}
                selectTitle={onlineActive
                  ? 'Active account'
                  : onlineSelectable
                    ? 'Launch with this Microsoft account'
                    : 'Online launch is not ready'}
                onSelect={onlineActive ? undefined : () => void useMode('online')}
                menuItems={microsoftMenuItems}
              />
            ) : (
              <IdentityRow
                head={(
                  <span class="cp-account-row__addhead">
                    <Icon name="globe" size={17} color="var(--text-mute)" />
                  </span>
                )}
                name="Sign in with Microsoft"
                detail={status?.login_available === false
                  ? status.login_reason
                  : 'Apply skins and launch online'}
                active={false}
                disabled={busy || Boolean(login) || status?.login_available === false}
                selectTitle="Start Microsoft device-code sign-in"
                onSelect={() => void startLogin()}
              />
            )}

            <IdentityRow
              head={(
                <PlayerHeadPreview
                  username={savedUsername}
                  textureSrc={accountTextureSrc}
                  size={38}
                  radius={9}
                  ariaLabel={`${savedUsername} offline head`}
                />
              )}
              name={savedUsername}
              detail="Offline identity"
              active={!onlineActive}
              disabled={busy && !onlineActive}
              selectTitle={onlineActive ? 'Launch with the offline identity' : 'Active identity'}
              onSelect={onlineActive ? () => void useMode('offline') : undefined}
              menuItems={offlineMenuItems}
            />
          </div>

          {login && (
            <DeviceCodePanel
              login={login}
              pollHint={pollHint}
              onCancel={() => {
                setLogin(null);
                setPollHint(null);
              }}
            />
          )}

          {message && (
            <div class="cp-account-message" data-tone={message.tone}>
              {message.text}
            </div>
          )}
        </ModalContent>
      </Modal>
    </>
  );
}
