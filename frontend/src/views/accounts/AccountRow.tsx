import type { JSX } from 'preact';
import { useLayoutEffect, useRef, useState } from 'preact/hooks';
import { accountsNotice, accountsOp, accountsSnapshot, selectAccount } from '../../machines/accounts';
import { Notice } from '../../ui/Atoms';
import { openContextMenu } from '../../ui/ContextMenu';
import { Icon } from '../../ui/Icons';
import { Popover } from '../../ui/Popover';
import { tipProps } from '../../ui/Tip';
import { AccountHead, AccountKindChip, AccountKindMark, accountName } from './account-face';
import { AccountAdders, AccountList, accountMenuItems, ACCOUNTS_UNAVAILABLE, accountSwitch } from './account-rows';

/**
 * The accounts on one line, never two. Each is a pill (head, name, kind) while they all fit; when they do not, the
 * ones not being played as shrink to their heads, each named in a tooltip; when the heads do not fit either, the
 * rest go behind "+N", which opens them as a list. The line ends with the way to add an account.
 */
export function AccountRow(): JSX.Element {
  const snapshot = accountsSnapshot.value;
  const accounts = snapshot.accounts;
  const busy = accountsOp.value !== null || snapshot.state !== 'ready';
  const notice = accountsNotice.value ?? (snapshot.state === 'unavailable' ? ACCOUNTS_UNAVAILABLE : null);

  const rowRef = useRef<HTMLDivElement>(null);
  const [layout, setLayout] = useState({ compact: false, hidden: 0 });
  const [open, setOpen] = useState<'more' | 'add' | null>(null);
  const moreRef = useRef<HTMLButtonElement>(null);
  const addRef = useRef<HTMLButtonElement>(null);
  const close = (): void => setOpen(null);
  const toggle = (kind: 'more' | 'add'): void => setOpen(open === kind ? null : kind);

  // The last `hidden` accounts that are not the active one sit behind "+N".
  const idle = accounts.filter((account) => !account.active);
  const behind = new Set(idle.slice(idle.length - layout.hidden));

  const key = accounts.map((account) => `${account.account_id}:${accountName(account)}:${account.active}`).join('|');
  useLayoutEffect(() => {
    const row = rowRef.current;
    if (!row) return undefined;
    const line = row.firstElementChild as HTMLElement;
    const spills = (): boolean => line.scrollWidth > line.clientWidth + 0.5;
    // Tried on the live row before it paints: every pill, then heads, then one head fewer until it fits.
    const arrange = (): void => {
      const pills = [...line.querySelectorAll<HTMLElement>('[data-account]')];
      const more = line.querySelector<HTMLElement>('[data-more]');
      const show = (compact: boolean, hidden: number): void => {
        line.dataset.compact = String(compact);
        const candidates = pills.filter((pill) => pill.getAttribute('aria-selected') !== 'true');
        candidates.forEach((pill, index) => {
          pill.hidden = index >= candidates.length - hidden;
        });
        if (more) more.hidden = hidden === 0;
      };
      let next = { compact: false, hidden: 0 };
      show(false, 0);
      if (spills()) {
        next = { compact: true, hidden: 0 };
        show(true, 0);
        while (spills() && next.hidden < idle.length) {
          next = { compact: true, hidden: next.hidden + 1 };
          show(true, next.hidden);
        }
      }
      setLayout((current) => (current.compact === next.compact && current.hidden === next.hidden ? current : next));
    };
    arrange();
    const observer = new ResizeObserver(arrange);
    observer.observe(row);
    return () => observer.disconnect();
  }, [key]);

  return (
    <div class="cp-accounts" ref={rowRef}>
      <div class="cp-accounts-line" role="tablist" aria-label="Accounts" data-compact={layout.compact}>
        {accounts.map((account) => {
          const name = accountName(account);
          const { ok, title } = accountSwitch(account);
          const headOnly = layout.compact && !account.active;
          return (
            <button
              key={account.account_id}
              type="button"
              role="tab"
              class="cp-account"
              data-account
              aria-selected={account.active}
              aria-label={`${name}, ${account.kind === 'microsoft' ? 'Microsoft' : 'Offline'}`}
              hidden={behind.has(account)}
              disabled={busy || (!account.active && !ok)}
              title={!headOnly && !account.active ? title : undefined}
              onClick={() => {
                if (!account.active) void selectAccount(account);
              }}
              onContextMenu={(event) => openContextMenu(event, accountMenuItems(account))}
              {...(headOnly
                ? tipProps(
                    <>
                      {name}
                      <AccountKindChip account={account} />
                    </>,
                  )
                : {})}
            >
              <AccountHead account={account} />
              <strong>{name}</strong>
              {account.kind === 'microsoft' && (
                <span class="cp-account-kind">
                  <AccountKindMark account={account} />
                </span>
              )}
            </button>
          );
        })}
        <span class="cp-accounts-anchor" data-more hidden={layout.hidden === 0}>
          <button
            ref={moreRef}
            type="button"
            class="cp-account cp-account-more"
            aria-haspopup="listbox"
            aria-expanded={open === 'more'}
            aria-label={`${layout.hidden} more accounts`}
            onClick={() => toggle('more')}
          >
            +{layout.hidden}
          </button>
          {open === 'more' && (
            <Popover anchor={moreRef} onDismiss={close} class="cp-pop cp-menu cp-accounts-pop">
              <AccountList accounts={[...behind]} label="More accounts" onSwitched={close} />
            </Popover>
          )}
        </span>
        <span class="cp-accounts-anchor">
          <button
            ref={addRef}
            type="button"
            class="cp-account cp-account-add"
            aria-haspopup="menu"
            aria-expanded={open === 'add'}
            onClick={() => toggle('add')}
          >
            <span class="cp-account-glyph">
              <Icon name="plus" size={14} />
            </span>
            Add account
          </button>
          {open === 'add' && (
            <Popover anchor={addRef} onDismiss={close} class="cp-pop cp-menu cp-accounts-pop">
              <AccountAdders onDone={close} />
            </Popover>
          )}
        </span>
      </div>
      {notice && (
        <Notice tone="error" role="alert">
          <p>{notice}</p>
        </Notice>
      )}
    </div>
  );
}
