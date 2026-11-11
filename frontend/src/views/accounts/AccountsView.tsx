import type { JSX } from 'preact';
import { useEffect } from 'preact/hooks';
import { accountsSnapshot, activeAccount, refreshAccountsData, renameOfflineAccount } from '../../machines/accounts';
import { refreshWardrobe, setWardrobeContext } from '../../machines/skin-wardrobe';
import { FALLBACK_SKIN_ACCOUNT_KEY, launcherSkinAccountKey } from '../../player-skin';
import { config } from '../../store';
import { SavedSkinLibrary } from './SavedSkinLibrary';

export function AccountsView(): JSX.Element {
  const snapshot = accountsSnapshot.value;
  const active = activeAccount(snapshot);
  const account = active?.kind === 'microsoft' ? active : null;
  const playerName = active?.display_name || config.value?.username || 'Player';
  const skinAccountKey = active ? launcherSkinAccountKey(active.account_id) : FALLBACK_SKIN_ACCOUNT_KEY;
  const skinAction = snapshot.state === 'ready' ? active?.skin_action : undefined;
  const skinActionsEnabled = skinAction?.enabled === true;

  useEffect(() => {
    void refreshAccountsData();
    void refreshWardrobe();
  }, []);

  useEffect(() => {
    setWardrobeContext({
      accountKey: skinAccountKey,
      selectionRevision: snapshot.selection_revision,
      skinActionsEnabled,
      account,
    });
  }, [skinAccountKey, snapshot.selection_revision, skinActionsEnabled, account]);

  return (
    <div class="cp-skinhall">
      <SavedSkinLibrary
        key={skinAccountKey}
        skinActionDisabledReason={skinAction?.disabled_reason || 'Online Minecraft account required'}
        playerName={playerName}
        onRenameNametag={active?.kind === 'offline' ? () => void renameOfflineAccount(active) : undefined}
      />
    </div>
  );
}
