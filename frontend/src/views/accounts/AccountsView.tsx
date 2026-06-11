import type { JSX } from 'preact';
import { promptPlayerName, savePlayerName } from '../../player-name';
import { refreshAccountSkin } from '../../player-skin';
import { config } from '../../store';
import { AccountSwitcher } from './AccountSwitcher';
import { useAuthStatus } from './hooks';
import { SavedSkinLibrary } from './SavedSkinLibrary';

export function AccountsView(): JSX.Element {
  const cfg = config.value;
  const savedUsername = cfg?.username || 'Player';
  const { status, state, refresh } = useAuthStatus(savedUsername);
  const onlineReady = state === 'ready' && Boolean(status?.online_mode_ready);
  const onlineActive = (cfg?.launch_auth_mode ?? 'offline') === 'online';
  const profileName = status?.minecraft_profile?.name;
  const playerName = onlineActive && profileName ? profileName : savedUsername;
  const renameNametag = onlineActive && profileName
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
          savedUsername={savedUsername}
          onChanged={() => {
            refresh();
            refreshAccountSkin();
          }}
        />
      </div>

      <SavedSkinLibrary
        onlineReady={onlineReady}
        minecraftProfile={status?.minecraft_profile}
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
