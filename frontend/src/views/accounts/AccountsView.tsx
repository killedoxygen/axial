import type { JSX } from 'preact';
import { promptPlayerName, savePlayerName } from '../../player-name';
import { FALLBACK_SKIN_ACCOUNT_KEY, launcherSkinAccountKey, refreshAccountSkin } from '../../player-skin';
import { config } from '../../store';
import { AccountSwitcher } from './AccountSwitcher';
import { useAuthStatus, useLauncherAccounts } from './hooks';
import { SavedSkinLibrary } from './SavedSkinLibrary';

export function AccountsView(): JSX.Element {
  const cfg = config.value;
  const savedUsername = cfg?.username || 'Player';
  const { status, state, refresh } = useAuthStatus(savedUsername);
  const accountsState = useLauncherAccounts();
  const activeAccount = accountsState.accounts.find((account) => account.active) ?? null;
  const onlineActive = activeAccount?.kind === 'microsoft';
  const onlineReady = state === 'ready' && Boolean(status?.skin_action?.enabled);
  const minecraftProfile = onlineActive ? (activeAccount?.minecraft_profile ?? status?.minecraft_profile) : undefined;
  const profileName = minecraftProfile?.name;
  const playerName = activeAccount?.display_name || (onlineActive && profileName ? profileName : savedUsername);
  const skinAccountKey = activeAccount ? launcherSkinAccountKey(activeAccount.account_id) : FALLBACK_SKIN_ACCOUNT_KEY;
  const renameNametag =
    onlineActive && profileName
      ? undefined
      : async (): Promise<void> => {
          const next = await promptPlayerName(savedUsername);
          if (!next) return;
          const saved = await savePlayerName(next);
          if (saved) refresh();
        };

  return (
    <div class="cp-view-page" style={{ gap: 18 }}>
      <div class="cp-page-header">
        <div>
          <h1>Skins</h1>
          <div class="cp-page-sub">Preview, fetch, and apply Minecraft skins.</div>
        </div>
        <AccountSwitcher
          status={status}
          state={state}
          accounts={accountsState.accounts}
          onChanged={() => {
            refresh();
            accountsState.refresh();
            refreshAccountSkin();
          }}
        />
      </div>

      <SavedSkinLibrary
        onlineReady={onlineReady}
        minecraftProfile={minecraftProfile}
        skinAccountKey={skinAccountKey}
        playerName={playerName}
        onRenameNametag={renameNametag ? () => void renameNametag() : undefined}
        onApplied={() => {
          refresh();
          refreshAccountSkin();
        }}
      />
    </div>
  );
}
