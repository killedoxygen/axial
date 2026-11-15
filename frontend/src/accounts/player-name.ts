import { prompt } from '../ui/Dialog';

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

export const USERNAME_MIN_LEN = 3;
export const USERNAME_MAX_LEN = 16;
export const USERNAME_PATTERN: RegExp = /^[A-Za-z0-9_]+$/;

export function validateUsername(raw: string): string | null {
  const v = raw.trim();
  if (v.length === 0) return 'Enter a name.';
  if (v.length < USERNAME_MIN_LEN) return `At least ${USERNAME_MIN_LEN} characters.`;
  if (v.length > USERNAME_MAX_LEN) return `At most ${USERNAME_MAX_LEN} characters.`;
  if (!USERNAME_PATTERN.test(v)) return 'Letters, numbers, and underscores only.';
  return null;
}
