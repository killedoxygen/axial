import { prompt } from './ui/Dialog';
import { USERNAME_MAX_LEN, validateUsername } from './utils';

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

