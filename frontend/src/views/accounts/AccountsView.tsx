import type { JSX } from 'preact';
import { useEffect } from 'preact/hooks';
import { accountsSnapshot, activeAccount, refreshAccountsData, renameOfflineIdentity } from '../../machines/accounts';
import { loadDefaultSkinKeys, refreshWardrobe, setWardrobeContext } from '../../machines/skin-wardrobe';
import { FALLBACK_SKIN_ACCOUNT_KEY, launcherSkinAccountKey } from '../../player-skin';
import { config } from '../../store';
import { SavedSkinLibrary } from './SavedSkinLibrary';

export function AccountsView(): JSX.Element {
  const savedUsername = config.value?.username || 'Player';
  const snapshot = accountsSnapshot.value;
  const active = activeAccount(snapshot);
  const onlineActive = active?.kind === 'microsoft';
  const skinAction = snapshot.state === 'ready' ? snapshot.status?.skin_action : undefined;
  const minecraftProfile = onlineActive ? active.minecraft_profile : undefined;
  const profileName = minecraftProfile?.name;
  const playerName = active?.display_name || (onlineActive && profileName ? profileName : savedUsername);
  const skinAccountKey = active ? launcherSkinAccountKey(active.account_id) : FALLBACK_SKIN_ACCOUNT_KEY;
  const skinActionsEnabled = skinAction?.enabled === true;
  const skinActionDisabledReason =
    skinAction?.disabled_reason || skinAction?.detail || 'Online Minecraft account required';

  useEffect(() => {
    void refreshAccountsData();
    void refreshWardrobe();
    loadDefaultSkinKeys();
  }, []);

  useEffect(() => {
    setWardrobeContext({
      accountKey: skinAccountKey,
      selectionRevision: snapshot.selection_revision,
      skinActionsEnabled,
      profile: minecraftProfile ?? null,
    });
  }, [skinAccountKey, snapshot.selection_revision, skinActionsEnabled, minecraftProfile]);

  const renameNametag = active?.kind === 'offline' ? () => renameOfflineIdentity(active) : undefined;

  return (
    <div class="cp-skinhall">
      <SavedSkinLibrary
        key={skinAccountKey}
        skinActionDisabledReason={skinActionDisabledReason}
        playerName={playerName}
        onRenameNametag={renameNametag ? () => void renameNametag() : undefined}
      />
    </div>
  );
}
