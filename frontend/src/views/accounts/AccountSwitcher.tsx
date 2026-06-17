import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { setConfig } from '../../actions';
import { api, isApiError } from '../../api';
import { hasNativeDesktopRuntime } from '../../native';
import {
  FALLBACK_SKIN_ACCOUNT_KEY,
  accountSkinSrc,
  minecraftProfileSkinTextureSrc,
  launcherSkinAccountKey,
  refreshAccountSkin,
  selectedSkinForAccount,
  selectedSkinTextureSrc,
} from '../../player-skin';
import { promptNewPlayerName, promptPlayerName } from '../../player-name';
import { IconButton } from '../../ui/Atoms';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { showConfirm } from '../../ui/Dialog';
import { Icon } from '../../ui/Icons';
import { MicrosoftMark } from '../../ui/MicrosoftMark';
import { Modal, ModalContent, ModalHeader, ModalTitle } from '../../ui/Modal';
import { PlayerHeadPreview } from '../../ui/PlayerHeadPreview';
import { authProfileSyncErrorMessage, authRefreshErrorMessage, configErrorMessage, logoutErrorMessage } from './auth';
import { commandSummary, isRecord, launcherAccountsResponse } from './api';
import type { AccountActionState, AuthStatusRecord, AuthStatusState, LauncherAccount } from './types';
import { useMicrosoftSignIn } from './useMicrosoftSignIn';

async function refreshConfigSignal(): Promise<void> {
  try {
    setConfig(await api('GET', '/config'));
  } catch (err: unknown) {
    console.warn('Could not refresh config after account change.', err);
  }
}

function actionEnabled(action: AccountActionState | undefined): boolean {
  return action?.enabled === true;
}

function actionUnavailableMessage(action: AccountActionState | undefined, fallback: string): string {
  return action?.disabled_reason || action?.detail || fallback;
}

function actionSuccessMessage(action: AccountActionState | undefined, fallback: string): string {
  return action?.success_summary || action?.label || fallback;
}

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
        onContextMenu={
          menuItems && menuItems.length > 0
            ? (event) => {
                event.preventDefault();
                openContextMenu(event, menuItems);
              }
            : undefined
        }
      >
        {head}
        <span class="cp-account-row__id">
          <span class="cp-account-row__name">{name}</span>
          <span class="cp-account-row__detail">{detail}</span>
        </span>
        <span class="cp-account-row__state">{active && <Icon name="check-circle" size={16} />}</span>
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

function AccountSection({
  title,
  emptyText,
  children,
}: {
  title: string;
  emptyText?: string;
  children: JSX.Element | JSX.Element[];
}): JSX.Element {
  const items = Array.isArray(children) ? children : [children];
  return (
    <section class="cp-account-section" aria-label={title}>
      <div class="cp-account-section__title">{title}</div>
      <div class="cp-account-section__rows">
        {items.length > 0 ? items : <div class="cp-account-section__empty">{emptyText}</div>}
      </div>
    </section>
  );
}

function AccountActionButton({
  head,
  name,
  detail,
  disabled,
  onSelect,
}: {
  head: JSX.Element;
  name: string;
  detail: string;
  disabled?: boolean;
  onSelect: () => void;
}): JSX.Element {
  return (
    <button type="button" class="cp-account-action" disabled={disabled} onClick={onSelect}>
      {head}
      <span class="cp-account-row__id">
        <span class="cp-account-row__name">{name}</span>
        <span class="cp-account-row__detail">{detail}</span>
      </span>
    </button>
  );
}

export function AccountSwitcher({
  status,
  state,
  accounts,
  onChanged,
  open: controlledOpen,
  onOpenChange,
  showTrigger = true,
}: {
  status: AuthStatusRecord | null;
  state: AuthStatusState;
  accounts: LauncherAccount[];
  onChanged: () => void;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  showTrigger?: boolean;
}): JSX.Element {
  const [internalOpen, setInternalOpen] = useState(false);
  const [removeBusy, setRemoveBusy] = useState(false);
  const [refreshBusy, setRefreshBusy] = useState(false);
  const [profileSyncBusy, setProfileSyncBusy] = useState(false);
  const [selectBusy, setSelectBusy] = useState(false);
  const open = controlledOpen ?? internalOpen;
  const setOpen = (next: boolean): void => {
    onOpenChange?.(next);
    if (controlledOpen === undefined) setInternalOpen(next);
  };

  const activeAccount = accounts.find((account) => account.active) ?? null;
  const activeMicrosoftAccount = activeAccount?.kind === 'microsoft' ? activeAccount : null;
  const activeProfile = activeMicrosoftAccount?.minecraft_profile ?? status?.minecraft_profile;
  const activeProfileName = activeProfile?.name ?? activeMicrosoftAccount?.display_name ?? '';
  const onlineActive = activeAccount?.kind === 'microsoft';
  const activeOfflineAccount = activeAccount?.kind === 'offline' ? activeAccount : null;
  const activeOfflineKey = activeOfflineAccount
    ? launcherSkinAccountKey(activeOfflineAccount.account_id)
    : FALLBACK_SKIN_ACCOUNT_KEY;
  const activeProfileTextureSrc = minecraftProfileSkinTextureSrc(activeProfile) ?? undefined;
  const offlineTextureSrc = selectedSkinTextureSrc(selectedSkinForAccount(activeOfflineKey)) ?? undefined;
  const liveAccountTextureSrc = accountSkinSrc.value ?? undefined;
  const chipName = activeAccount?.display_name || activeProfileName || 'Select account';
  const chipHeadName = activeAccount ? chipName : 'Player';
  const accountTextureSrc = activeAccount
    ? (liveAccountTextureSrc ?? (onlineActive && activeProfileTextureSrc ? activeProfileTextureSrc : offlineTextureSrc))
    : undefined;
  const activeRefreshAction = activeMicrosoftAccount?.refresh_action ?? status?.refresh_action;
  const activeProfileSyncAction = activeMicrosoftAccount?.profile_sync_action ?? status?.profile_sync_action;
  const refreshAvailable = actionEnabled(activeRefreshAction);
  const profileSyncAvailable = actionEnabled(activeProfileSyncAction);
  const microsoftAccounts = accounts.filter((account) => account.kind === 'microsoft');
  const offlineAccounts = accounts.filter((account) => account.kind === 'offline');
  const externalBusy = removeBusy || refreshBusy || profileSyncBusy || selectBusy;
  const microsoftSignInAvailable = hasNativeDesktopRuntime() || status?.login_available !== false;
  const loginFlow = useMicrosoftSignIn({
    canStart: !externalBusy && microsoftSignInAvailable,
    onAuthenticated: async (result) => {
      const latest = launcherAccountsResponse(await api('GET', '/accounts'));
      if (!latest) throw new Error('Account list could not be read after Microsoft sign-in.');

      const signedInLoginId =
        typeof result.login_id === 'string' && result.login_id.trim() ? result.login_id.trim() : null;
      const signedIn = signedInLoginId
        ? (latest.accounts.find((account) => account.kind === 'microsoft' && account.login_id === signedInLoginId) ??
          null)
        : (latest.accounts.find((account) => account.active && account.kind === 'microsoft') ?? null);

      let active = signedIn?.active
        ? signedIn
        : signedInLoginId
          ? null
          : (latest.accounts.find((account) => account.active && account.kind === 'microsoft') ?? null);
      if (signedIn && !signedIn.active) {
        const selected = await api('POST', `/accounts/${encodeURIComponent(signedIn.account_id)}/select`);
        if (isRecord(selected) && typeof selected.error === 'string') {
          await refreshConfigSignal();
          onChanged();
          refreshAccountSkin();
          return { tone: 'err', text: configErrorMessage(selected) };
        }
        const selectedLatest = launcherAccountsResponse(await api('GET', '/accounts'));
        active =
          selectedLatest?.accounts.find(
            (account) => account.kind === 'microsoft' && account.login_id === signedIn.login_id && account.active,
          ) ?? null;
      }

      if (!active || active.online_action?.state_id !== 'online_ready') {
        await refreshConfigSignal();
        onChanged();
        refreshAccountSkin();
        return {
          tone: 'err',
          text: actionUnavailableMessage(
            active?.online_action,
            'Microsoft sign-in completed, but account state is unavailable.',
          ),
        };
      }
      try {
        await api('POST', '/skins/from-profile', { mark_current: true });
      } catch (err: unknown) {
        console.warn('Could not seed profile skin after Microsoft sign-in.', err);
      }
      await refreshConfigSignal();
      onChanged();
      refreshAccountSkin();
      return { tone: 'ok', text: actionSuccessMessage(active.online_action, 'Account state updated.') };
    },
  });
  const busy = externalBusy || loginFlow.busy;

  const refreshAuth = async (): Promise<void> => {
    if (busy) return;
    setRefreshBusy(true);
    loginFlow.clearMessage();
    try {
      const response = await api('POST', '/auth/refresh');
      if (isRecord(response) && typeof response.error === 'string') {
        loginFlow.setMessage({ tone: 'err', text: authRefreshErrorMessage(response) });
        return;
      }
      if (!isRecord(response)) throw new Error('Microsoft sign-in refresh returned an invalid response.');
      loginFlow.setMessage({
        tone: 'ok',
        text: commandSummary(response, actionSuccessMessage(activeRefreshAction, 'Account state updated.')),
      });
      await refreshConfigSignal();
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
    if (busy || !profileSyncAvailable) return;
    setProfileSyncBusy(true);
    loginFlow.clearMessage();
    try {
      const response = await api('POST', '/auth/profile/sync');
      if (isRecord(response) && typeof response.error === 'string') {
        loginFlow.setMessage({ tone: 'err', text: authProfileSyncErrorMessage(response) });
        return;
      }
      if (!isRecord(response)) throw new Error('Minecraft profile sync returned an invalid response.');
      loginFlow.setMessage({
        tone: 'ok',
        text: commandSummary(response, actionSuccessMessage(activeProfileSyncAction, 'Account state updated.')),
      });
      await refreshConfigSignal();
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

  const selectAccount = async (account: LauncherAccount): Promise<void> => {
    if (busy || account.active) return;
    if (account.kind === 'microsoft' && !actionEnabled(account.online_action)) {
      loginFlow.setMessage({
        tone: 'err',
        text: actionUnavailableMessage(
          account.online_action,
          'This Microsoft account is not available for Online mode.',
        ),
      });
      return;
    }
    setSelectBusy(true);
    loginFlow.clearMessage();
    try {
      const selected = await api('POST', `/accounts/${encodeURIComponent(account.account_id)}/select`);
      if (isRecord(selected) && typeof selected.error === 'string') {
        loginFlow.setMessage({ tone: 'err', text: configErrorMessage(selected) });
        return;
      }
      loginFlow.setMessage({ tone: 'ok', text: commandSummary(selected, 'Account selected.') });
      await refreshConfigSignal();
      onChanged();
      refreshAccountSkin();
    } catch (err: unknown) {
      loginFlow.setMessage({
        tone: 'err',
        text: isApiError(err)
          ? configErrorMessage(err.payload)
          : 'Could not reach the local backend to switch account.',
      });
    } finally {
      setSelectBusy(false);
    }
  };

  const createOffline = async (): Promise<void> => {
    const next = await promptNewPlayerName();
    if (!next || busy) return;
    setSelectBusy(true);
    loginFlow.clearMessage();
    try {
      const response = await api('POST', '/accounts/offline', { username: next });
      if (isRecord(response) && typeof response.error === 'string') {
        loginFlow.setMessage({ tone: 'err', text: configErrorMessage(response) });
        return;
      }
      loginFlow.setMessage({ tone: 'ok', text: commandSummary(response, 'Offline identity created.') });
      await refreshConfigSignal();
      onChanged();
      refreshAccountSkin();
    } catch (err: unknown) {
      loginFlow.setMessage({
        tone: 'err',
        text: isApiError(err)
          ? configErrorMessage(err.payload)
          : 'Could not reach the local backend to create offline identity.',
      });
    } finally {
      setSelectBusy(false);
    }
  };

  const renameOffline = async (account: LauncherAccount): Promise<void> => {
    const next = await promptPlayerName(account.display_name);
    if (!next || busy) return;
    setSelectBusy(true);
    loginFlow.clearMessage();
    try {
      const response = await api('PATCH', `/accounts/${encodeURIComponent(account.account_id)}`, { username: next });
      if (isRecord(response) && typeof response.error === 'string') {
        loginFlow.setMessage({ tone: 'err', text: configErrorMessage(response) });
        return;
      }
      loginFlow.setMessage({ tone: 'ok', text: commandSummary(response, 'Offline identity updated.') });
      await refreshConfigSignal();
      onChanged();
      refreshAccountSkin();
    } catch (err: unknown) {
      loginFlow.setMessage({
        tone: 'err',
        text: isApiError(err)
          ? configErrorMessage(err.payload)
          : 'Could not reach the local backend to rename offline identity.',
      });
    } finally {
      setSelectBusy(false);
    }
  };

  const removeAccount = async (account: LauncherAccount): Promise<void> => {
    if (busy) return;
    const actionText = account.kind === 'microsoft' && account.active ? 'Sign out' : 'Remove';
    const ok = await showConfirm(`${actionText} ${account.display_name} from this launcher?`, {
      title:
        account.kind === 'microsoft' ? (account.active ? 'Sign out' : 'Remove Microsoft account') : 'Remove identity',
      destructive: true,
      confirmText: actionText,
    });
    if (!ok) return;

    setRemoveBusy(true);
    loginFlow.clearMessage();
    try {
      const response = await api('DELETE', `/accounts/${encodeURIComponent(account.account_id)}`);
      if (isRecord(response) && typeof response.error === 'string') {
        loginFlow.setMessage({ tone: 'err', text: logoutErrorMessage(response) });
        return;
      }
      loginFlow.setMessage({ tone: 'ok', text: commandSummary(response, 'Account removed.') });
      await refreshConfigSignal();
      onChanged();
      refreshAccountSkin();
    } catch (err: unknown) {
      loginFlow.setMessage({
        tone: 'err',
        text: isApiError(err)
          ? logoutErrorMessage(err.payload)
          : 'Could not reach the local backend to remove account.',
      });
    } finally {
      setRemoveBusy(false);
    }
  };

  const microsoftMenuItems = (account: LauncherAccount): ContextMenuItem[] => [
    ...(account.active && profileSyncAvailable
      ? [
          {
            icon: 'refresh',
            label: activeProfileSyncAction?.label ?? 'Sync Minecraft profile',
            onSelect: () => void syncMinecraftProfile(),
          },
        ]
      : []),
    ...(account.active && refreshAvailable
      ? [
          {
            icon: 'refresh',
            label: activeRefreshAction?.label ?? 'Refresh Microsoft sign-in',
            onSelect: () => void refreshAuth(),
          },
        ]
      : []),
    ...(microsoftSignInAvailable
      ? [{ icon: 'globe', label: 'Re-verify with Microsoft', onSelect: () => void loginFlow.startLogin() }]
      : []),
    { label: '', onSelect: () => {}, divider: true },
    account.active
      ? { icon: 'x', label: 'Sign out', onSelect: () => void removeAccount(account), danger: true }
      : { icon: 'x', label: 'Remove account', onSelect: () => void removeAccount(account), danger: true },
  ];

  const offlineMenuItems = (account: LauncherAccount): ContextMenuItem[] => [
    { icon: 'edit', label: 'Rename', onSelect: () => void renameOffline(account) },
    { icon: 'x', label: 'Remove identity', onSelect: () => void removeAccount(account), danger: true },
  ];

  return (
    <>
      {showTrigger && (
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
            username={chipHeadName}
            textureSrc={accountTextureSrc}
            size={30}
            radius={7}
            ariaLabel={`${chipName} account`}
          />
          <span class="cp-account-chip__id">
            <span class="cp-account-chip__name">{state === 'loading' ? 'Loading' : chipName}</span>
            <span class="cp-account-chip__mode">
              {activeAccount ? (onlineActive ? 'Microsoft account' : 'Offline identity') : 'No account selected'}
            </span>
          </span>
          <Icon name="chevron-down" size={14} color="var(--text-dim)" />
        </button>
      )}

      <Modal open={open} onOpenChange={setOpen}>
        <ModalContent className="cp-account-modal" aria-label="Accounts" aria-describedby={undefined}>
          <ModalHeader>
            <ModalTitle>Accounts</ModalTitle>
          </ModalHeader>
          <div class="cp-account-rows">
            <AccountSection title="Online" emptyText="No Microsoft accounts added.">
              {microsoftAccounts.map((account) => {
                const profile = account.minecraft_profile;
                const profileName = (profile?.name ?? account.display_name) || 'Microsoft account';
                const onlineSelectable = actionEnabled(account.online_action);
                const profileTextureSrc = account.active
                  ? (liveAccountTextureSrc ?? minecraftProfileSkinTextureSrc(profile) ?? undefined)
                  : (minecraftProfileSkinTextureSrc(profile) ?? undefined);
                return (
                  <IdentityRow
                    key={account.account_id}
                    head={
                      <PlayerHeadPreview
                        username={profileName || 'Player'}
                        textureSrc={profileTextureSrc}
                        size={38}
                        radius={9}
                        ariaLabel={`${profileName || 'Microsoft'} profile head`}
                      />
                    }
                    name={profileName}
                    detail={account.view_model?.detail ?? account.online_action?.detail ?? 'Microsoft account'}
                    active={account.active}
                    disabled={busy || (!account.active && !onlineSelectable)}
                    selectTitle={
                      account.active
                        ? 'Active account'
                        : onlineSelectable
                          ? 'Launch with this Microsoft account'
                          : actionUnavailableMessage(account.online_action, 'Account unavailable')
                    }
                    onSelect={account.active ? undefined : () => void selectAccount(account)}
                    menuItems={microsoftMenuItems(account)}
                  />
                );
              })}
            </AccountSection>

            <AccountSection title="Offline" emptyText="No offline identities added.">
              {offlineAccounts.map((account) => {
                const textureSrc = account.active
                  ? (liveAccountTextureSrc ??
                    selectedSkinTextureSrc(selectedSkinForAccount(launcherSkinAccountKey(account.account_id))) ??
                    undefined)
                  : (selectedSkinTextureSrc(selectedSkinForAccount(launcherSkinAccountKey(account.account_id))) ??
                    undefined);
                return (
                  <IdentityRow
                    key={account.account_id}
                    head={
                      <PlayerHeadPreview
                        username={account.display_name}
                        textureSrc={textureSrc}
                        size={38}
                        radius={9}
                        ariaLabel={`${account.display_name} offline head`}
                      />
                    }
                    name={account.display_name}
                    detail="Offline identity"
                    active={account.active}
                    disabled={busy}
                    selectTitle={account.active ? 'Active identity' : 'Launch with this offline identity'}
                    onSelect={account.active ? undefined : () => void selectAccount(account)}
                    menuItems={offlineMenuItems(account)}
                  />
                );
              })}
            </AccountSection>

            <div class="cp-account-actions" aria-label="Add account">
              <AccountActionButton
                head={
                  <span class="cp-account-row__addhead">
                    <MicrosoftMark size={18} />
                  </span>
                }
                name={microsoftAccounts.length === 0 ? 'Sign in with Microsoft' : 'Add Microsoft account'}
                detail={
                  !microsoftSignInAvailable
                    ? (status?.login_reason ?? 'Microsoft sign-in is unavailable.')
                    : microsoftAccounts.length === 0
                      ? 'Apply skins and launch online'
                      : 'Sign in another Minecraft account'
                }
                disabled={busy || !microsoftSignInAvailable}
                onSelect={() => void loginFlow.startLogin()}
              />
              <AccountActionButton
                head={
                  <span class="cp-account-row__addhead">
                    <Icon name="plus" size={17} color="var(--text-mute)" />
                  </span>
                }
                name="New offline identity"
                detail="Create local username"
                disabled={busy}
                onSelect={() => void createOffline()}
              />
            </div>
          </div>

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
