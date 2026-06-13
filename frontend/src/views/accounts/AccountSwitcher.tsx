import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
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
  logoutErrorMessage,
  authProfileSyncErrorMessage,
  statusCanSelectOnline,
  authRefreshErrorMessage,
  type AuthLoginPending,
} from './auth';
import type { AuthStatusRecord, AuthStatusState } from './types';
import { useMicrosoftDeviceLogin } from './useMicrosoftDeviceLogin';

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
  const [logoutBusy, setLogoutBusy] = useState(false);
  const [refreshBusy, setRefreshBusy] = useState(false);
  const [profileSyncBusy, setProfileSyncBusy] = useState(false);
  const [modeBusy, setModeBusy] = useState<LaunchAuthMode | null>(null);

  const msaActive = Boolean(status?.msa_authenticated);
  const profile = status?.minecraft_profile;
  const profileName = profile?.name ?? (msaActive ? status?.username ?? '' : '');
  const onlineSelectable = status ? statusCanSelectOnline(status) : false;
  const savedMode = launchAuthMode(config.value?.launch_auth_mode ?? status?.launch_auth_mode);
  const onlineActive = savedMode === 'online';
  const refreshAvailable = Boolean(status?.login_available && status?.msa_refresh_available);
  const profileSyncAvailable = Boolean(profile);
  const profileTextureSrc = profile ? apiResourceUrl('/skin/profile/file') : undefined;
  localStateVersion.value;
  const accountTextureSrc = selectedSkinTextureSrc() ?? undefined;
  const externalBusy = logoutBusy || refreshBusy || profileSyncBusy || modeBusy !== null;
  const loginFlow = useMicrosoftDeviceLogin({
    canStart: !externalBusy && status?.login_available !== false,
    onAuthenticated: () => {
      onChanged();
    },
  });
  const busy = externalBusy || loginFlow.busy;
  const chipName = onlineActive && profileName ? profileName : savedUsername;

  const logout = async (): Promise<void> => {
    if (busy) return;
    const ok = await showConfirm(
      'Sign out of the Microsoft account? Offline launches stay available.',
      { title: 'Sign out', destructive: true, confirmText: 'Sign out' },
    );
    if (!ok) return;
    setLogoutBusy(true);
    loginFlow.cancelLogin();
    loginFlow.clearMessage();
    try {
      const response = await api('POST', '/auth/logout');
      if (isRecord(response) && typeof response.error === 'string') {
        loginFlow.setMessage({ tone: 'err', text: logoutErrorMessage(response) });
      } else {
        loginFlow.setMessage({ tone: 'ok', text: 'Microsoft sign-in cleared.' });
      }
    } catch (err: unknown) {
      loginFlow.setMessage({
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
    if (busy || loginFlow.login) return;
    setRefreshBusy(true);
    loginFlow.clearMessage();
    try {
      const response = await api('POST', '/auth/refresh');
      if (isRecord(response) && typeof response.error === 'string') {
        loginFlow.setMessage({ tone: 'err', text: authRefreshErrorMessage(response) });
        return;
      }
      if (!isRecord(response) || response.status !== 'refreshed') {
        loginFlow.setMessage({ tone: 'err', text: 'Microsoft sign-in refresh returned an unexpected response.' });
        return;
      }
      loginFlow.setMessage({ tone: 'ok', text: 'Microsoft sign-in refreshed.' });
    } catch (err: unknown) {
      loginFlow.setMessage({
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

  const syncMinecraftProfile = async (): Promise<void> => {
    if (busy || loginFlow.login || !profileSyncAvailable) return;
    setProfileSyncBusy(true);
    loginFlow.clearMessage();
    try {
      const response = await api('POST', '/auth/profile/sync');
      if (isRecord(response) && typeof response.error === 'string') {
        loginFlow.setMessage({ tone: 'err', text: authProfileSyncErrorMessage(response) });
        return;
      }
      if (!isRecord(response) || response.status !== 'profile_synced') {
        loginFlow.setMessage({ tone: 'err', text: 'Minecraft profile sync returned an unexpected response.' });
        return;
      }
      loginFlow.setMessage({ tone: 'ok', text: 'Minecraft profile synced.' });
    } catch (err: unknown) {
      loginFlow.setMessage({
        tone: 'err',
        text: isApiError(err)
          ? authProfileSyncErrorMessage(err.payload)
          : 'Could not reach the local backend to sync Minecraft profile.',
      });
    } finally {
      onChanged();
      setProfileSyncBusy(false);
    }
  };

  const useMode = async (nextMode: LaunchAuthMode): Promise<void> => {
    if (busy || nextMode === savedMode) return;
    if (nextMode === 'online' && !onlineSelectable) {
      loginFlow.setMessage({
        tone: 'err',
        text: 'Online needs a verified, Java-owning Minecraft account with valid credentials.',
      });
      return;
    }
    setModeBusy(nextMode);
    loginFlow.clearMessage();
    try {
      const response = await api('PUT', '/config', { launch_auth_mode: nextMode });
      if (isRecord(response) && typeof response.error === 'string') {
        loginFlow.setMessage({ tone: 'err', text: configErrorMessage(response) });
        return;
      }
      config.value = response;
      onChanged();
    } catch {
      loginFlow.setMessage({ tone: 'err', text: 'Could not reach the local backend to save launch mode.' });
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
    ...(profileSyncAvailable
      ? [{ icon: 'refresh', label: 'Sync Minecraft profile', onSelect: () => void syncMinecraftProfile() }]
      : []),
    ...(refreshAvailable
      ? [{ icon: 'refresh', label: 'Refresh credentials', onSelect: () => void refreshAuth() }]
      : []),
    ...(status?.login_available
      ? [{ icon: 'globe', label: 'Re-verify with device code', onSelect: () => void loginFlow.startLogin() }]
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
          loginFlow.clearMessage();
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
                disabled={busy || Boolean(loginFlow.login) || status?.login_available === false}
                selectTitle="Start Microsoft device-code sign-in"
                onSelect={() => void loginFlow.startLogin()}
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

          {loginFlow.login && (
            <DeviceCodePanel
              login={loginFlow.login}
              pollHint={loginFlow.pollHint}
              onCancel={() => {
                loginFlow.cancelLogin();
              }}
            />
          )}

          {loginFlow.message && (
            <div class="cp-account-message" data-tone={loginFlow.message.tone}>
              {loginFlow.message.text}
            </div>
          )}
        </ModalContent>
      </Modal>
    </>
  );
}
