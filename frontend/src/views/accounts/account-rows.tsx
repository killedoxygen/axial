import type { JSX } from 'preact';
import {
  accountsOp,
  accountsSnapshot,
  actionEnabled,
  actionUnavailableMessage,
  createOfflineAccount,
  microsoftSignInAvailable,
  refreshMicrosoftAuth,
  removeAccount,
  renameOfflineAccount,
  selectAccount,
  signInWithMicrosoftAccount,
  syncMinecraftProfile,
} from '../../machines/accounts';
import { openContextMenu, type ContextMenuItem } from '../../ui/ContextMenu';
import { Icon } from '../../ui/Icons';
import { MicrosoftMark } from '../../ui/MicrosoftMark';
import { AccountHead, AccountKindChip, accountName } from './account-face';
import type { LauncherAccount } from './types';

export const SIGN_IN_UNAVAILABLE = 'Microsoft sign-in is available in the desktop app.';
export const ACCOUNTS_UNAVAILABLE = 'Accounts are unavailable. Check that the local backend is running.';

/** Whether the account can be switched to, and what to say when it cannot. */
export function accountSwitch(account: LauncherAccount): { ok: boolean; title: string } {
  const ok = account.kind === 'offline' || actionEnabled(account.online_action);
  return {
    ok,
    title: ok
      ? `Switch to ${accountName(account)}`
      : actionUnavailableMessage(account.online_action, 'Account unavailable'),
  };
}

/** Everything that can be done to an account besides switching to it. */
export function accountMenuItems(account: LauncherAccount): ContextMenuItem[] {
  if (account.kind === 'offline') {
    return [
      { icon: 'edit', label: 'Rename', onSelect: () => void renameOfflineAccount(account) },
      { icon: 'x', label: 'Remove identity', onSelect: () => void removeAccount(account), danger: true },
    ];
  }
  const items: ContextMenuItem[] = [];
  if (account.active && actionEnabled(account.profile_sync_action)) {
    items.push({
      icon: 'refresh',
      label: account.profile_sync_action.label || 'Sync Minecraft profile',
      onSelect: () => void syncMinecraftProfile(),
    });
  }
  if (account.active && actionEnabled(account.refresh_action)) {
    items.push({
      icon: 'refresh',
      label: account.refresh_action.label || 'Refresh Microsoft sign-in',
      onSelect: () => void refreshMicrosoftAuth(),
    });
  }
  if (microsoftSignInAvailable()) {
    items.push({ icon: 'globe', label: 'Re-verify with Microsoft', onSelect: () => void signInWithMicrosoftAccount() });
  }
  if (items.length > 0) items.push({ label: '', onSelect: () => {}, divider: true });
  items.push({
    icon: 'x',
    label: account.active ? 'Sign out' : 'Remove account',
    onSelect: () => void removeAccount(account),
    danger: true,
  });
  return items;
}

/** Accounts as a list: switch on click, the rest from the context menu. */
export function AccountList({
  accounts,
  label,
  onSwitched,
}: {
  accounts: LauncherAccount[];
  label: string;
  onSwitched?: () => void;
}): JSX.Element {
  const busy = accountsOp.value !== null || accountsSnapshot.value.state !== 'ready';
  return (
    <div class="cp-account-list" role="listbox" aria-label={label}>
      {accounts.map((account) => {
        const { ok, title } = accountSwitch(account);
        return (
          <button
            key={account.account_id}
            type="button"
            class="cp-menu-item"
            role="option"
            aria-selected={account.active}
            disabled={busy || (!account.active && !ok)}
            title={account.active ? undefined : title}
            onClick={() => {
              if (!account.active) void selectAccount(account).then((switched) => switched && onSwitched?.());
            }}
            onContextMenu={(event) => openContextMenu(event, accountMenuItems(account))}
          >
            <AccountHead account={account} />
            <strong>{accountName(account)}</strong>
            {account.active && <Icon name="check" size={14} stroke={2.6} />}
            <AccountKindChip account={account} />
          </button>
        );
      })}
    </div>
  );
}

/** The two ways to add an account. */
export function AccountAdders({ onDone }: { onDone?: () => void }): JSX.Element {
  const snapshot = accountsSnapshot.value;
  const busy = accountsOp.value !== null || snapshot.state !== 'ready';
  const signInAvailable = microsoftSignInAvailable(snapshot);
  return (
    <div class="cp-account-adders" role="group" aria-label="Add an account">
      <button
        type="button"
        class="cp-menu-item"
        disabled={busy || !signInAvailable}
        title={signInAvailable ? undefined : SIGN_IN_UNAVAILABLE}
        onClick={() => {
          onDone?.();
          void signInWithMicrosoftAccount();
        }}
      >
        <span class="cp-account-glyph">
          <MicrosoftMark size={14} />
        </span>
        {accountsOp.value === 'sign-in' ? 'Waiting for Microsoft sign-in' : 'Add Microsoft account'}
      </button>
      <button
        type="button"
        class="cp-menu-item"
        disabled={busy}
        onClick={() => {
          onDone?.();
          void createOfflineAccount();
        }}
      >
        <span class="cp-account-glyph">
          <Icon name="plus" size={14} />
        </span>
        New offline identity
      </button>
    </div>
  );
}
