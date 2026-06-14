import { signal } from '@preact/signals';
import { api, apiResourceUrl } from './api';
import { DEFAULT_SKINS } from './default-skins';
import { local, saveLocalState } from './state';
import { config } from './store';

export const accountSkinSrc = signal<string | null>(null);
export const accountDisplayName = signal('Player');

const DEFAULT_SELECTED_SKIN = 'default:steve';
export const FALLBACK_SKIN_ACCOUNT_KEY = 'account:fallback';

type MinecraftProfileSkin = {
  id?: unknown;
  state?: unknown;
  url?: unknown;
};

type MinecraftProfileLike = {
  id?: unknown;
  name?: unknown;
  skins?: unknown;
};

type LauncherAccountLike = {
  active?: unknown;
  account_id?: unknown;
  kind?: unknown;
  display_name?: unknown;
  minecraft_profile?: unknown;
};

type AuthStatusLike = {
  launch_auth_mode?: unknown;
  online_mode_ready?: unknown;
  msa_refresh_available?: unknown;
  username?: unknown;
  minecraft_profile?: unknown;
};

type LauncherAccountsLike = {
  accounts?: unknown;
};

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

export function minecraftProfileSkinTextureSrc(profile: MinecraftProfileLike | undefined | null): string | null {
  const id = typeof profile?.id === 'string' ? profile.id.trim() : '';
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

  void applyAccountSkinFromAccounts(requestId, fallbackName, false).catch(() => {
    void applyAccountSkinFromAuthStatus(requestId, fallbackName, false).catch(() => {
      if (requestId === accountSkinRequestId) applyNoAccountHead(fallbackName);
    });
  });
}

async function applyAccountSkinFromAccounts(
  requestId: number,
  fallbackName: string,
  refreshAttempted: boolean,
): Promise<void> {
  const response = await api('GET', '/accounts');
  if (requestId !== accountSkinRequestId) return;
  const payload = launcherAccountsLike(response);
  if (!payload || !Array.isArray(payload.accounts)) {
    await applyAccountSkinFromAuthStatus(requestId, fallbackName, refreshAttempted);
    return;
  }
  const activeAccount = payload.accounts
    .map(launcherAccountLike)
    .find((account): account is LauncherAccountLike => Boolean(account?.active));
  if (!activeAccount) {
    await applyAccountSkinFromAuthStatus(requestId, fallbackName, refreshAttempted);
    return;
  }

  const displayName =
    typeof activeAccount.display_name === 'string' && activeAccount.display_name.trim()
      ? activeAccount.display_name.trim()
      : fallbackName;
  if (activeAccount.kind === 'microsoft') {
    const profile = minecraftProfileLike(activeAccount.minecraft_profile);
    if (profile) {
      accountDisplayName.value =
        typeof profile.name === 'string' && profile.name.trim() ? profile.name.trim() : displayName;
      accountSkinSrc.value = minecraftProfileSkinTextureSrc(profile);
      return;
    }
    if (!refreshAttempted) {
      await api('POST', '/auth/refresh');
      await applyAccountSkinFromAccounts(requestId, fallbackName, true);
      return;
    }
  }

  if (activeAccount.kind === 'offline' && typeof activeAccount.account_id === 'string') {
    accountDisplayName.value = displayName;
    accountSkinSrc.value = selectedSkinTextureSrc(
      selectedSkinForAccount(launcherSkinAccountKey(activeAccount.account_id)),
    );
    return;
  }

  applyNoAccountHead(fallbackName);
}

async function applyAccountSkinFromAuthStatus(
  requestId: number,
  fallbackName: string,
  refreshAttempted: boolean,
): Promise<void> {
  const response = await api('GET', '/auth/status');
  if (requestId !== accountSkinRequestId) return;
  const status = authStatusLike(response);
  if (!status) {
    applyNoAccountHead(fallbackName);
    return;
  }

  const profile = activeStatusMinecraftProfile(status);
  if (status.launch_auth_mode === 'online' && profile) {
    const profileName = typeof profile.name === 'string' && profile.name.trim() ? profile.name.trim() : fallbackName;
    accountDisplayName.value = profileName;
    accountSkinSrc.value = minecraftProfileSkinTextureSrc(profile);
    if (!status.online_mode_ready && status.msa_refresh_available === true && !refreshAttempted) {
      void refreshAuthAndApplyAccountSkin(requestId, fallbackName);
    }
    return;
  }

  if (status.launch_auth_mode === 'online' && status.msa_refresh_available === true && !refreshAttempted) {
    await api('POST', '/auth/refresh');
    await applyAccountSkinFromAuthStatus(requestId, fallbackName, true);
    return;
  }

  applyNoAccountHead(authStatusDisplayName(status, fallbackName));
}

async function refreshAuthAndApplyAccountSkin(requestId: number, fallbackName: string): Promise<void> {
  try {
    await api('POST', '/auth/refresh');
    await applyAccountSkinFromAuthStatus(requestId, fallbackName, true);
  } catch (err: unknown) {
    // Keep the restored profile visible. Launch/auth flows surface refresh errors
    // when the user takes an online action.
    console.warn('Could not refresh restored Microsoft sign-in for the account head.', err);
  }
}

function activeStatusMinecraftProfile(status: AuthStatusLike): MinecraftProfileLike | null {
  return minecraftProfileLike(status.minecraft_profile);
}

function launcherAccountLike(value: unknown): LauncherAccountLike | null {
  if (!value || typeof value !== 'object') return null;
  return value as LauncherAccountLike;
}

function applyNoAccountHead(displayName = 'Player'): void {
  accountDisplayName.value = displayName.trim() || 'Player';
  accountSkinSrc.value = selectedSkinTextureSrc(selectedSkinForAccount(FALLBACK_SKIN_ACCOUNT_KEY));
}

function authStatusDisplayName(status: AuthStatusLike, fallbackName: string): string {
  return typeof status.username === 'string' && status.username.trim() ? status.username.trim() : fallbackName;
}

function validSelectedSkin(value: string | undefined): string {
  const selected = value?.trim();
  return selected || DEFAULT_SELECTED_SKIN;
}

function activeMinecraftSkin(profile: MinecraftProfileLike | undefined | null): { id: string; url: string } | null {
  const skins = Array.isArray(profile?.skins) ? profile.skins : [];
  const parsed = skins
    .map(minecraftProfileSkin)
    .filter((skin): skin is { id: string; state: string; url: string } => skin !== null);
  const selected = parsed.find((skin) => skin.state.toLowerCase() === 'active') ?? parsed[0];
  if (!selected) return null;
  return { id: selected.id, url: selected.url };
}

function minecraftProfileSkin(value: unknown): { id: string; state: string; url: string } | null {
  if (!value || typeof value !== 'object') return null;
  const skin = value as MinecraftProfileSkin;
  if (typeof skin.id !== 'string' || typeof skin.state !== 'string' || typeof skin.url !== 'string') {
    return null;
  }
  return {
    id: skin.id.trim(),
    state: skin.state.trim(),
    url: skin.url.trim(),
  };
}

function minecraftProfileLike(value: unknown): MinecraftProfileLike | null {
  if (!value || typeof value !== 'object') return null;
  return value as MinecraftProfileLike;
}

function authStatusLike(value: unknown): AuthStatusLike | null {
  if (!value || typeof value !== 'object') return null;
  return value as AuthStatusLike;
}

function launcherAccountsLike(value: unknown): LauncherAccountsLike | null {
  if (!value || typeof value !== 'object') return null;
  return value as LauncherAccountsLike;
}
