import type { JSX } from 'preact';
import { useEffect } from 'preact/hooks';
import { refreshAccountsData } from '../../machines/accounts';
import { Modal, ModalContent } from '../../ui/Modal';
import { accountSwitcherAnchor, accountSwitcherOpen, closeAccountSwitcher } from '../../ui-state';
import { AccountSwitcherPanel } from './AccountSwitcher';

const PANEL_WIDTH = 336;
const VIEWPORT_GUTTER = 12;

export function AccountSwitcherHost(): JSX.Element | null {
  const open = accountSwitcherOpen.value;

  useEffect(() => {
    if (open) void refreshAccountsData();
  }, [open]);

  if (!open) return null;

  const anchor = accountSwitcherAnchor.value;
  const anchoredStyle = anchor
    ? {
        top: `${Math.max(VIEWPORT_GUTTER, Math.min(anchor.y, window.innerHeight - 220))}px`,
        left: `${Math.max(VIEWPORT_GUTTER, Math.min(anchor.x - PANEL_WIDTH, window.innerWidth - PANEL_WIDTH - VIEWPORT_GUTTER))}px`,
        width: `${PANEL_WIDTH}px`,
      }
    : undefined;

  return (
    <Modal
      open={open}
      onOpenChange={(next) => {
        if (!next) closeAccountSwitcher();
      }}
    >
      <ModalContent
        className={anchor ? 'cp-acct-surface cp-acct-surface--pop' : 'cp-acct-surface cp-acct-surface--modal'}
        style={anchoredStyle}
        aria-label="Accounts"
        aria-describedby={undefined}
        showCloseButton={false}
      >
        <AccountSwitcherPanel />
      </ModalContent>
    </Modal>
  );
}
