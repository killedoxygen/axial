import type { JSX } from 'preact';
import { useEffect } from 'preact/hooks';
import { accountsNotice, accountsSnapshot, refreshAccountsData } from '../../machines/accounts';
import { Notice } from '../../ui/Atoms';
import { Modal, ModalContent } from '../../ui/Modal';
import { accountSwitcherOpen, closeAccountSwitcher } from '../../ui-state';
import { AccountAdders, AccountList, ACCOUNTS_UNAVAILABLE } from './account-rows';

/** Switching account from anywhere: every account, then the ways to add one, hanging beside the rail's account button. */
export function AccountSwitcher(): JSX.Element | null {
  const open = accountSwitcherOpen.value;
  useEffect(() => {
    if (open) void refreshAccountsData();
  }, [open]);
  if (!open) return null;

  const snapshot = accountsSnapshot.value;
  const notice = accountsNotice.value ?? (snapshot.state === 'unavailable' ? ACCOUNTS_UNAVAILABLE : null);
  return (
    <Modal open onOpenChange={(next) => !next && closeAccountSwitcher()}>
      <ModalContent className="cp-pop cp-menu cp-account-switcher" aria-label="Accounts" showCloseButton={false}>
        {snapshot.accounts.length > 0 && (
          <AccountList accounts={snapshot.accounts} label="Accounts" onSwitched={closeAccountSwitcher} />
        )}
        <AccountAdders onDone={closeAccountSwitcher} />
        {notice && (
          <Notice tone="error" role="alert">
            <p>{notice}</p>
          </Notice>
        )}
      </ModalContent>
    </Modal>
  );
}
