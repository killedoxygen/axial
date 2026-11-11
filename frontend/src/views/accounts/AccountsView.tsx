import type { JSX } from 'preact';
import { useEffect } from 'preact/hooks';
import { accountsSnapshot, activeAccount, refreshAccountsData } from '../../machines/accounts';
import { refreshWardrobe, setWardrobeContext } from '../../machines/skin-wardrobe';
import { FALLBACK_SKIN_ACCOUNT_KEY, launcherSkinAccountKey } from '../../player-skin';
import { AccountRow } from './AccountRow';
import { Wardrobe } from './Wardrobe';

/** The accounts on one line, then the wardrobe of the one being played as, down to the page's bottom edge. */
export function AccountsView(): JSX.Element {
  const snapshot = accountsSnapshot.value;
  const active = activeAccount(snapshot);
  const account = active?.kind === 'microsoft' ? active : null;
  const accountKey = active ? launcherSkinAccountKey(active.account_id) : FALLBACK_SKIN_ACCOUNT_KEY;
  const skinAction = snapshot.state === 'ready' ? active?.skin_action : undefined;
  const skinActionsEnabled = skinAction?.enabled === true;

  useEffect(() => {
    void refreshAccountsData();
    void refreshWardrobe();
  }, []);

  useEffect(() => {
    setWardrobeContext({ accountKey, selectionRevision: snapshot.selection_revision, skinActionsEnabled, account });
  }, [accountKey, snapshot.selection_revision, skinActionsEnabled, account]);

  return (
    <div class="cp-view-page cp-accounts-page">
      <AccountRow />
      <Wardrobe
        key={accountKey}
        skinActionDisabledReason={skinAction?.disabled_reason || 'Online Minecraft account required'}
      />
    </div>
  );
}
