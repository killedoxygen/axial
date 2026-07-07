import type { JSX } from 'preact';
import {
  accountsNotice,
  accountsOp,
  accountsSnapshot,
  actionEnabled,
  activeAccount,
  activeMicrosoftProfileSyncAction,
  activeMicrosoftRefreshAction,
  createOfflineIdentity,
  microsoftSignInAvailable,
  refreshMicrosoftAuth,
  removeAccount,
  renameOfflineIdentity,
  selectAccount,
  signInWithMicrosoftAccount,
  syncMinecraftProfile,
} from '../../machines/accounts';
import {
  accountSkinSrc,
  launcherSkinAccountKey,
  minecraftProfileSkinTextureSrc,
  selectedSkinForAccount,
  selectedSkinTextureSrc,
} from '../../player-skin';
import { IconButton } from '../../ui/Atoms';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { Icon } from '../../ui/Icons';
import { MicrosoftMark } from '../../ui/MicrosoftMark';
import { PlayerHeadPreview } from '../../ui/PlayerHeadPreview';
import { accountSwitcherAnchor, expandAccountSwitcher, openAccountSwitcher } from '../../ui-state';
import { actionUnavailableMessage } from '../../machines/accounts';
import type { LauncherAccount } from './types';

function accountTextureSrc(account: LauncherAccount, live: boolean): string | undefined {
  if (live && accountSkinSrc.value) return accountSkinSrc.value;
  if (account.kind === 'microsoft') {
    return minecraftProfileSkinTextureSrc(account.minecraft_profile) ?? undefined;
  }
  return selectedSkinTextureSrc(selectedSkinForAccount(launcherSkinAccountKey(account.account_id))) ?? undefined;
}

function accountDisplayLabel(account: LauncherAccount): string {
  if (account.kind === 'microsoft') {
    return (account.minecraft_profile?.name ?? account.display_name) || 'Microsoft account';
  }
  return account.display_name;
}

function accountDetailLabel(account: LauncherAccount): string {
  if (account.kind === 'microsoft') {
    return account.view_model?.detail ?? account.online_action?.detail ?? 'Microsoft account';
  }
  return 'Offline identity';
}

function offlineAccountMenuItems(account: LauncherAccount): ContextMenuItem[] {
  return [
    { icon: 'edit', label: 'Rename', onSelect: () => void renameOfflineIdentity(account) },
    { icon: 'x', label: 'Remove identity', onSelect: () => void removeAccount(account), danger: true },
  ];
}

function activeAccountMenuItems(account: LauncherAccount): ContextMenuItem[] {
  if (account.kind === 'offline') return offlineAccountMenuItems(account);
  const syncAction = activeMicrosoftProfileSyncAction();
  const refreshAction = activeMicrosoftRefreshAction();
  return [
    ...(actionEnabled(syncAction)
      ? [
          {
            icon: 'refresh',
            label: syncAction?.label ?? 'Sync Minecraft profile',
            onSelect: () => void syncMinecraftProfile(),
          },
        ]
      : []),
    ...(actionEnabled(refreshAction)
      ? [
          {
            icon: 'refresh',
            label: refreshAction?.label ?? 'Refresh Microsoft sign-in',
            onSelect: () => void refreshMicrosoftAuth(),
          },
        ]
      : []),
    ...(microsoftSignInAvailable()
      ? [{ icon: 'globe', label: 'Re-verify with Microsoft', onSelect: () => void signInWithMicrosoftAccount() }]
      : []),
    { label: '', onSelect: () => {}, divider: true },
    { icon: 'x', label: 'Sign out', onSelect: () => void removeAccount(account), danger: true },
  ];
}

function idleAccountMenuItems(account: LauncherAccount): ContextMenuItem[] {
  if (account.kind === 'offline') return offlineAccountMenuItems(account);
  return [
    ...(microsoftSignInAvailable()
      ? [{ icon: 'globe', label: 'Re-verify with Microsoft', onSelect: () => void signInWithMicrosoftAccount() }]
      : []),
    { icon: 'x', label: 'Remove account', onSelect: () => void removeAccount(account), danger: true },
  ];
}

function SwitchRow({ account, busy }: { account: LauncherAccount; busy: boolean }): JSX.Element {
  const name = accountDisplayLabel(account);
  const selectable = account.kind === 'offline' || actionEnabled(account.online_action);
  const menuItems = idleAccountMenuItems(account);
  return (
    <div class="cp-acct__rowwrap">
      <button
        type="button"
        class="cp-acct__row"
        disabled={busy || !selectable}
        title={
          selectable ? `Switch to ${name}` : actionUnavailableMessage(account.online_action, 'Account unavailable')
        }
        onClick={() => void selectAccount(account)}
        onContextMenu={(event) => {
          event.preventDefault();
          openContextMenu(event, menuItems);
        }}
      >
        <PlayerHeadPreview
          username={name || 'Player'}
          textureSrc={accountTextureSrc(account, false)}
          size={34}
          radius={9}
          ariaLabel={`${name} head`}
        />
        <span class="cp-acct__row-id">
          <span class="cp-acct__row-name">{name}</span>
          <span class="cp-acct__row-detail">{accountDetailLabel(account)}</span>
        </span>
      </button>
      <span class="cp-acct__row-menu">
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
    </div>
  );
}

export function AccountSwitcherPanel(): JSX.Element {
  const snapshot = accountsSnapshot.value;
  const busy = accountsOp.value !== null;
  const signingIn = accountsOp.value === 'sign-in';
  const active = activeAccount(snapshot);
  const others = snapshot.accounts.filter((account) => !account.active);
  const microsoftCount = snapshot.accounts.filter((account) => account.kind === 'microsoft').length;
  const signInAvailable = microsoftSignInAvailable(snapshot);
  const notice = accountsNotice.value;

  return (
    <div class="cp-acct" aria-label="Accounts">
      {active ? (
        <div class="cp-acct__me">
          <PlayerHeadPreview
            username={accountDisplayLabel(active) || 'Player'}
            textureSrc={accountTextureSrc(active, true)}
            size={44}
            radius={11}
            ariaLabel={`${accountDisplayLabel(active)} head`}
          />
          <span class="cp-acct__me-id">
            <span class="cp-acct__me-name">{accountDisplayLabel(active)}</span>
            <span class="cp-acct__me-detail">
              {active.kind === 'microsoft' && <span class="cp-acct__me-dot" aria-hidden="true" />}
              {accountDetailLabel(active)}
            </span>
          </span>
          <span class="cp-acct__me-actions">
            {accountSwitcherAnchor.value !== null && (
              <IconButton icon="expand" size={28} tooltip="Open full view" onClick={() => expandAccountSwitcher()} />
            )}
            <IconButton
              icon="dots"
              size={28}
              tooltip="Account actions"
              disabled={busy}
              onClick={(event) => openContextMenu(event, activeAccountMenuItems(active))}
            />
          </span>
        </div>
      ) : (
        <div class="cp-acct__me cp-acct__me--empty">
          <span class="cp-acct__me-id">
            <span class="cp-acct__me-name">
              {snapshot.state === 'loading' ? 'Loading accounts' : 'No account selected'}
            </span>
            <span class="cp-acct__me-detail">Pick an identity to launch with</span>
          </span>
        </div>
      )}

      {others.length > 0 && (
        <>
          <div class="cp-acct__sep" aria-hidden="true" />
          <div class="cp-acct__label">Switch to</div>
          <div class="cp-acct__rows">
            {others.map((account) => (
              <SwitchRow key={account.account_id} account={account} busy={busy} />
            ))}
          </div>
        </>
      )}

      <div class="cp-acct__sep" aria-hidden="true" />
      <button
        type="button"
        class="cp-acct__add"
        disabled={busy || !signInAvailable}
        onClick={() => void signInWithMicrosoftAccount()}
      >
        <span class="cp-acct__add-glyph">
          <MicrosoftMark size={16} />
        </span>
        <span class="cp-acct__row-id">
          <span class="cp-acct__row-name">
            {microsoftCount === 0 ? 'Sign in with Microsoft' : 'Add Microsoft account'}
          </span>
          <span class="cp-acct__row-detail">
            {signingIn
              ? 'Waiting for Microsoft sign-in'
              : !signInAvailable
                ? (snapshot.status?.login_reason ?? 'Microsoft sign-in is unavailable.')
                : microsoftCount === 0
                  ? 'Apply skins and launch online'
                  : 'Sign in another Minecraft account'}
          </span>
        </span>
      </button>
      <button type="button" class="cp-acct__add" disabled={busy} onClick={() => void createOfflineIdentity()}>
        <span class="cp-acct__add-glyph">
          <Icon name="plus" size={16} color="var(--text-mute)" />
        </span>
        <span class="cp-acct__row-id">
          <span class="cp-acct__row-name">New offline identity</span>
          <span class="cp-acct__row-detail">Create a local username</span>
        </span>
      </button>

      {notice && (
        <div class="cp-acct__notice" data-tone="err">
          {notice}
        </div>
      )}
      {snapshot.state === 'unavailable' && !notice && (
        <div class="cp-acct__notice" data-tone="err">
          Accounts are unavailable. Check that the local backend is running.
        </div>
      )}
    </div>
  );
}

export function AccountSwitcherChip(): JSX.Element {
  const snapshot = accountsSnapshot.value;
  const active = activeAccount(snapshot);
  const name = active ? accountDisplayLabel(active) : 'Select account';
  return (
    <button
      type="button"
      class="cp-account-chip"
      title="Switch account or identity"
      onClick={(event) => {
        const rect = event.currentTarget.getBoundingClientRect();
        openAccountSwitcher({ x: rect.right, y: rect.bottom + 8 });
      }}
    >
      <PlayerHeadPreview
        username={active ? name : 'Player'}
        textureSrc={active ? (accountSkinSrc.value ?? accountTextureSrc(active, true)) : undefined}
        size={30}
        radius={7}
        ariaLabel={`${name} account`}
      />
      <span class="cp-account-chip__id">
        <span class="cp-account-chip__name">{snapshot.state === 'loading' ? 'Loading' : name}</span>
        <span class="cp-account-chip__mode">
          {active ? (active.kind === 'microsoft' ? 'Microsoft account' : 'Offline identity') : 'No account selected'}
        </span>
      </span>
      <Icon name="chevron-down" size={14} color="var(--text-dim)" />
    </button>
  );
}
