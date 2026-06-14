import type { JSX } from 'preact';
import { refreshAccountSkin } from '../../player-skin';
import { config } from '../../store';
import { accountSwitcherOpen } from '../../ui-state';
import { AccountSwitcher } from './AccountSwitcher';
import { useAuthStatus, useLauncherAccounts } from './hooks';

export function AccountSwitcherHost(): JSX.Element | null {
  const savedUsername = config.value?.username || 'Player';
  const { status, state, refresh } = useAuthStatus(savedUsername);
  const accountsState = useLauncherAccounts();

  if (!accountSwitcherOpen.value) return null;

  return (
    <AccountSwitcher
      status={status}
      state={state}
      accounts={accountsState.accounts}
      open={accountSwitcherOpen.value}
      onOpenChange={(open) => {
        accountSwitcherOpen.value = open;
      }}
      showTrigger={false}
      onChanged={() => {
        refresh();
        accountsState.refresh();
        refreshAccountSkin();
      }}
    />
  );
}
