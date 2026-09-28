import { signal } from '@preact/signals';
import { apiResourceUrl } from './api';
import { DEFAULT_SKINS } from './default-skins';
import { accountsSnapshot, activeAccount } from './machines/accounts-state';
import { local, saveLocalState, canEditPreferences } from './state';
import { config } from './store';
import type { MinecraftProfile } from './views/accounts/types';

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

export function minecraftProfileSkinTextureSrc(profile: MinecraftProfile | undefined | null): string | null {
  const id = profile?.id.trim() ?? '';
  const skin = activeMinecraftSkin(profile);
  if (!id || !skin) return null;

  const params = new URLSearchParams({ profile: id });
  if (skin.id) params.set('skin', skin.id);
  if (skin.url) params.set('texture', skin.url);
  return apiResourceUrl(`/skin/profile/file?${params.toString()}`);
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

export function refreshAccountSkin(): void {
  const fallbackName = config.value?.username || 'Player';
  const account = activeAccount(accountsSnapshot.value);
  if (!account) {
    applyNoAccountHead(fallbackName);
    return;
  }

  const displayName = account.display_name.trim() || fallbackName;
  if (account.kind === 'microsoft') {
    const profile = account.minecraft_profile;
    if (profile) {
      accountDisplayName.value = profile.name.trim() || displayName;
      accountSkinSrc.value = minecraftProfileSkinTextureSrc(profile);
      return;
    }
  }

  if (account.kind === 'offline') {
    accountDisplayName.value = displayName;
    accountSkinSrc.value = selectedSkinTextureSrc(
      selectedSkinForAccount(launcherSkinAccountKey(account.account_id)),
    );
    return;
  }

  applyNoAccountHead(fallbackName);
}

function applyNoAccountHead(displayName = 'Player'): void {
  accountDisplayName.value = displayName.trim() || 'Player';
  accountSkinSrc.value = selectedSkinTextureSrc(selectedSkinForAccount(FALLBACK_SKIN_ACCOUNT_KEY));
}

function validSelectedSkin(value: string | undefined): string {
  const selected = value?.trim();
  return selected || DEFAULT_SELECTED_SKIN;
}

function activeMinecraftSkin(profile: MinecraftProfile | undefined | null): { id: string; url: string } | null {
  const selected = profile?.skins.find((skin) => skin.state.toLowerCase() === 'active') ?? profile?.skins[0];
  if (!selected) return null;
  return { id: selected.id, url: selected.url };
}
