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
    const { activeAccount: readActiveAccount, accountsNotice, refreshAccountsData, saveOfflineIdentityName } =
      await import('./machines/accounts');
    await refreshAccountsData();
    const activeAccount = readActiveAccount();
    if (activeAccount?.kind === 'microsoft') {
      toast('Microsoft account names are managed by Minecraft.', 'error');
      return false;
    }
    if (!activeAccount) throw new Error('Select an offline identity before changing its name.');
    const saved = await saveOfflineIdentityName(activeAccount, nextName, successMessage);
    if (!saved) throw new Error(accountsNotice.value ?? 'The account name could not be saved.');
    return saved;
  } catch (err) {
    toast(`Could not save player name: ${errMessage(err)}`, 'error');
    return false;
  }
}
