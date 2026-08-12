import { api } from './api';
import { refreshAccountSkin } from './player-skin';
import { config } from './store';
import { toast } from './toast';
import { prompt } from './ui/Dialog';
import { USERNAME_MAX_LEN, errMessage, validateUsername } from './utils';
import { configResponse } from './dto-core';
import { dtoError } from './dto-contract';
import { launcherAccountsResponse } from './views/accounts/api';
import type { LauncherAccount } from './views/accounts/types';

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

export async function savePlayerName(raw: string, successMessage = 'Player name updated'): Promise<boolean> {
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
      const response = await api('PATCH', `/accounts/${encodeURIComponent(activeAccount.account_id)}`, {
        username: nextName,
      });
      const error = dtoError(response);
      if (error) throw new Error(error);
      config.value = configResponse(await api('GET', '/config'));
    } else {
      config.value = configResponse(await api('PUT', '/config', { username: nextName }));
    }
    refreshAccountSkin();
    toast(successMessage);
    return true;
  } catch (err) {
    toast(`Could not save player name: ${errMessage(err)}`, 'error');
    return false;
  }
}

async function readActiveLauncherAccount(): Promise<LauncherAccount | null> {
  const response = await api('GET', '/accounts');
  const accounts = launcherAccountsResponse(response);
  if (!accounts) throw new Error('Launcher accounts response was invalid.');
  return accounts.accounts.find((account) => account.active) ?? null;
}
