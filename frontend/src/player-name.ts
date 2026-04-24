import { api } from './api';
import { config } from './store';
import { toast } from './toast';
import { prompt } from './ui/Dialog';
import { USERNAME_MAX_LEN, errMessage, validateUsername } from './utils';

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

export async function savePlayerName(
  raw: string,
  successMessage = 'Player name updated',
): Promise<boolean> {
  if (validateUsername(raw) !== null) return false;
  try {
    const res: any = await api('PUT', '/config', { username: raw.trim() });
    if (res.error) throw new Error(res.error);
    config.value = res;
    toast(successMessage);
    return true;
  } catch (err) {
    toast(`Failed: ${errMessage(err)}`, 'error');
    return false;
  }
}
