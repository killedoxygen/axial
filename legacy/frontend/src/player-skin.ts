import { signal } from '@preact/signals';
import { api, apiResourceUrl } from './api';
import { DEFAULT_SKINS } from './default-skins';
import { local, saveLocalState } from './state';
import { config } from './store';
import { authStatusResponse, launcherAccountsResponse } from './views/accounts/api';
import type { AuthStatusRecord, MinecraftProfile } from './views/accounts/types';

export const accountSkinSrc = signal<string | null>(null);
export const accountDisplayName = signal('Player');

const DEFAULT_SELECTED_SKIN = 'default:steve';
export const FALLBACK_SKIN_ACCOUNT_KEY = 'account:fallback';

let accountSkinRequestId = 0;

export function launcherSkinAccountKey(accountId: string): string {
  const normalized = accountId.trim().toLowerCase();
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
    return textureKey ? apiResourceUrl(`/skins/${textureKey}/file`) : null;
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
  const requestId = ++accountSkinRequestId;
  const fallbackName = config.value?.username || 'Player';

  void applyAccountSkinFromAccounts(requestId, fallbackName).catch(() => {
    if (requestId === accountSkinRequestId) applyNoAccountHead(fallbackName);
  });
}

async function applyAccountSkinFromAccounts(requestId: number, fallbackName: string): Promise<void> {
  const response = await api('GET', '/accounts');
  if (requestId !== accountSkinRequestId) return;
  const payload = launcherAccountsResponse(response);
  if (!payload) throw new Error('Launcher accounts response was invalid.');
  const activeAccount = payload.accounts.find((account) => account.active);
  if (!activeAccount) {
    await applyAccountSkinFromAuthStatus(requestId, fallbackName);
    return;
  }

  const displayName = activeAccount.display_name.trim() || fallbackName;
  if (activeAccount.kind === 'microsoft') {
    const profile = activeAccount.minecraft_profile;
    if (profile) {
      accountDisplayName.value = profile.name.trim() || displayName;
      accountSkinSrc.value = minecraftProfileSkinTextureSrc(profile);
      return;
    }
  }

  if (activeAccount.kind === 'offline') {
    accountDisplayName.value = displayName;
    accountSkinSrc.value = selectedSkinTextureSrc(
      selectedSkinForAccount(launcherSkinAccountKey(activeAccount.account_id)),
    );
    return;
  }

  applyNoAccountHead(fallbackName);
}

async function applyAccountSkinFromAuthStatus(requestId: number, fallbackName: string): Promise<void> {
  const response = await api('GET', '/auth/status');
  if (requestId !== accountSkinRequestId) return;
  const status = authStatusResponse(response);
  if (!status) throw new Error('Authentication status response was invalid.');

  const profile = status.minecraft_profile;
  if (status.launch_auth_mode === 'online' && profile) {
    const profileName = profile.name.trim() || fallbackName;
    accountDisplayName.value = profileName;
    accountSkinSrc.value = minecraftProfileSkinTextureSrc(profile);
    return;
  }

  applyNoAccountHead(authStatusDisplayName(status, fallbackName));
}

function applyNoAccountHead(displayName = 'Player'): void {
  accountDisplayName.value = displayName.trim() || 'Player';
  accountSkinSrc.value = selectedSkinTextureSrc(selectedSkinForAccount(FALLBACK_SKIN_ACCOUNT_KEY));
}

function authStatusDisplayName(status: AuthStatusRecord, fallbackName: string): string {
  return status.username.trim() || fallbackName;
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
