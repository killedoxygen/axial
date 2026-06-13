import { api } from './api';
import { refreshAccountSkin } from './player-skin';
import { config } from './store';
import { toast } from './toast';
import { prompt } from './ui/Dialog';
import { USERNAME_MAX_LEN, errMessage, validateUsername } from './utils';

type LauncherAccountLike = {
  account_id: string;
  kind: 'microsoft' | 'offline';
  active: boolean;
};

export function clampPlayerNameInput(value: string): string {
  return value.slice(0, USERNAME_MAX_LEN);
}

export async function promptPlayerName(current: string): Promise<string | null> {
  const next = await prompt('Display name', current, {
    title: 'Change name',
    placeholder: 'Your gamertag',
    confirmText: 'Save',
    validate: validateUsername,
    normalizeInput: clampPlayerNameInput,
  });
  if (!next || next === current) return null;
  return next;
}

export async function promptNewPlayerName(): Promise<string | null> {
  const next = await prompt('Display name', '', {
    title: 'New offline identity',
    placeholder: 'Your gamertag',
    confirmText: 'Create',
    validate: validateUsername,
    normalizeInput: clampPlayerNameInput,
  });
  return next || null;
}

export async function savePlayerName(
  raw: string,
  successMessage = 'Player name updated',
): Promise<boolean> {
  const validationError = validateUsername(raw);
  if (validationError !== null) {
    toast(`Invalid name: ${validationError}`, 'error');
    return false;
  }
  const nextName = raw.trim();
  try {
    const activeAccount = await readActiveLauncherAccount();
    if (activeAccount?.kind === 'microsoft') {
      toast('Microsoft account names are managed by Minecraft.', 'error');
      return false;
    }
    if (activeAccount?.kind === 'offline') {
      const response: any = await api(
        'PATCH',
        `/accounts/${encodeURIComponent(activeAccount.account_id)}`,
        { username: nextName },
      );
      if (response.error) throw new Error(response.error);
      config.value = await api('GET', '/config');
    } else {
      const res: any = await api('PUT', '/config', { username: nextName });
      if (res.error) throw new Error(res.error);
      config.value = res;
    }
    refreshAccountSkin();
    toast(successMessage);
    return true;
  } catch (err) {
    toast(`Could not save player name: ${errMessage(err)}`, 'error');
    return false;
  }
}

async function readActiveLauncherAccount(): Promise<LauncherAccountLike | null> {
  const response = await api('GET', '/accounts');
  if (!response || typeof response !== 'object' || !Array.isArray(response.accounts)) return null;
  for (const account of response.accounts) {
    const parsed = launcherAccountLike(account);
    if (parsed?.active) return parsed;
  }
  return null;
}

function launcherAccountLike(value: unknown): LauncherAccountLike | null {
  if (!value || typeof value !== 'object') return null;
  const account = value as Record<string, unknown>;
  if (
    typeof account.account_id !== 'string' ||
    (account.kind !== 'microsoft' && account.kind !== 'offline') ||
    typeof account.active !== 'boolean'
  ) {
    return null;
  }
  return {
    account_id: account.account_id,
    kind: account.kind,
    active: account.active,
  };
}
