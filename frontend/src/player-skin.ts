import { signal } from '@preact/signals';
import { apiResourceUrl } from './api';
import { DEFAULT_SKINS } from './default-skins';
import { accountsSnapshot, activeAccount } from './machines/accounts-state';
import { local, saveLocalState, canEditPreferences } from './state';
import { config } from './store';
import type { LauncherAccount, MinecraftCape } from './views/accounts/types';

export const accountSkinSrc = signal<string | null>(null);
export const accountDisplayName = signal('Player');

const DEFAULT_SELECTED_SKIN = 'default:steve';
export const FALLBACK_SKIN_ACCOUNT_KEY = 'account:fallback';

export function launcherSkinAccountKey(accountId: string): string {
  const normalized = accountId.trim();
  return `account:${normalized || 'unknown'}`;
}

export function selectedSkinForAccount(accountKey?: string): string {
  if (!accountKey) return validSelectedSkin(local.selectedSkin);
  return validSelectedSkin(local.selectedSkinsByAccount[accountKey] ?? local.selectedSkin);
}

export function hasSelectedSkinForAccount(accountKey: string): boolean {
  return (
    typeof local.selectedSkinsByAccount[accountKey] === 'string' &&
    local.selectedSkinsByAccount[accountKey].trim().length > 0
  );
}

export function selectedSkinTextureSrc(value = selectedSkinForAccount()): string | null {
  if (value.startsWith('default:')) {
    const id = value.slice('default:'.length);
    return DEFAULT_SKINS.find((skin) => skin.id === id)?.src ?? null;
  }
  if (value.startsWith('saved:')) {
    const textureKey = value.slice('saved:'.length);
    return textureKey ? apiResourceUrl(`/skins/${encodeURIComponent(textureKey)}/file`) : null;
  }
  return null;
}

/** The texture of the skin a Microsoft account's Minecraft profile wears. */
export function profileSkinTextureSrc(account: LauncherAccount): string | null {
  const profile = account.minecraft_profile;
  const skin = account.profile_skin;
  if (!profile || !skin) return null;
  const params = new URLSearchParams({ profile: profile.id, skin: skin.id, texture: skin.url });
  return apiResourceUrl(`/skin/profile/file?${params.toString()}`);
}

/** The cape a Microsoft account's Minecraft profile wears. */
export function profileCape(account: LauncherAccount | null | undefined): MinecraftCape | null {
  return account?.minecraft_profile?.capes.find((cape) => cape.id === account.profile_cape_id) ?? null;
}

export function setSelectedSkin(value: string, accountKey?: string): void {
  if (!canEditPreferences()) return;
  const next = validSelectedSkin(value);
  if (accountKey) {
    if (local.selectedSkinsByAccount[accountKey] !== next) {
      local.selectedSkinsByAccount = {
        ...local.selectedSkinsByAccount,
        [accountKey]: next,
      };
    }
  } else {
    local.selectedSkin = next;
  }
  saveLocalState();
  refreshAccountSkin();
}

export function resetSelectedSkin(accountKey?: string): void {
  setSelectedSkin(DEFAULT_SELECTED_SKIN, accountKey);
}

/** Carries an account's chosen skin to the id it has after a rename. */
export function moveAccountSkin(fromAccountId: string, toAccountId: string): void {
  const from = launcherSkinAccountKey(fromAccountId);
  const to = launcherSkinAccountKey(toAccountId);
  if (from === to || !canEditPreferences()) return;
  const { [from]: chosen, ...rest } = local.selectedSkinsByAccount;
  if (chosen === undefined) return;
  local.selectedSkinsByAccount = { ...rest, [to]: chosen };
  saveLocalState();
}

/** Drops the skin chosen for an account that no longer exists. */
export function forgetAccountSkin(accountId: string): void {
  const key = launcherSkinAccountKey(accountId);
  if (!(key in local.selectedSkinsByAccount) || !canEditPreferences()) return;
  const { [key]: _removed, ...rest } = local.selectedSkinsByAccount;
  local.selectedSkinsByAccount = rest;
  saveLocalState();
}

export function refreshAccountSkin(): void {
  const fallbackName = config.value?.username || 'Player';
  const account = activeAccount(accountsSnapshot.value);
  const profile = account?.kind === 'microsoft' ? account.minecraft_profile : null;
  if (!account || (account.kind === 'microsoft' && !profile)) {
    applyNoAccountHead(fallbackName);
    return;
  }
  accountDisplayName.value = profile?.name.trim() || account.display_name.trim() || fallbackName;
  accountSkinSrc.value = accountSkinTextureSrc(account);
}

/** The skin texture an account shows: its Minecraft profile skin, or the look chosen for it in Axial. */
export function accountSkinTextureSrc(account: LauncherAccount): string | null {
  return account.kind === 'microsoft'
    ? profileSkinTextureSrc(account)
    : selectedSkinTextureSrc(selectedSkinForAccount(launcherSkinAccountKey(account.account_id)));
}

function applyNoAccountHead(displayName = 'Player'): void {
  accountDisplayName.value = displayName.trim() || 'Player';
  accountSkinSrc.value = selectedSkinTextureSrc(selectedSkinForAccount(FALLBACK_SKIN_ACCOUNT_KEY));
}

function validSelectedSkin(value: string | undefined): string {
  const selected = value?.trim();
  return selected || DEFAULT_SELECTED_SKIN;
}
