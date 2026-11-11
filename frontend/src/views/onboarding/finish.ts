import { api } from '../../api';
import { DEFAULT_SKINS } from '../../default-skins';
import { dtoError } from '../../dto-contract';
import { configResponse } from '../../dto-core';
import { saveConfigPatch } from '../../hooks/use-autosave';
import {
  accountsNotice,
  accountsSnapshot,
  activeAccount,
  createOfflineAccount,
  refreshAccountsData,
  selectAccount,
} from '../../machines/accounts';
import { Music } from '../../music';
import { launcherSkinAccountKey, refreshAccountSkin, setSelectedSkin } from '../../player-skin';
import { config, systemInfo } from '../../store';
import { getMemoryRecommendation } from '../../utils';
import { saveLookupSkin } from '../accounts/api';
import type { Choice, Look } from './use-identity';

export interface Preferences {
  music: boolean;
  discord: boolean;
  telemetry: boolean;
}

const MUSIC_VOLUME = 5;

/** Query that binds the profile skin seed to the Microsoft account selected right now. */
function profileSeedQuery(): URLSearchParams | null {
  const { state, accounts, selection_revision } = accountsSnapshot.value;
  const account = accounts.find((candidate) => candidate.active && candidate.kind === 'microsoft');
  if (state !== 'ready' || !account || selection_revision === null) return null;
  return new URLSearchParams({
    expected_account_id: account.account_id,
    expected_selection_revision: String(selection_revision),
  });
}

async function selectedSkinFor(look: Look): Promise<string> {
  if (!look.lookup) return `default:${look.id}`;
  try {
    return `saved:${(await saveLookupSkin(look.lookup.username, look.lookup.variant)).texture_key}`;
  } catch (err: unknown) {
    console.warn('Could not save the looked-up skin during onboarding.', err);
    return `default:${DEFAULT_SKINS[0].id}`;
  }
}

async function activateOfflineAccount({ name, look, lookChosen }: Extract<Choice, { kind: 'offline' }>): Promise<void> {
  await refreshAccountsData();
  const snapshot = accountsSnapshot.value;
  if (snapshot.state !== 'ready') throw new Error('Could not load accounts. Try again.');
  const existing = snapshot.accounts.find((account) => account.kind === 'offline' && account.display_name === name);
  if (existing) {
    if (!existing.active) await selectAccount(existing);
    if (activeAccount()?.account_id !== existing.account_id) {
      throw new Error(accountsNotice.value ?? 'Could not select the offline account. Try again.');
    }
  } else if (!(await createOfflineAccount(name))) {
    throw new Error(accountsNotice.value ?? 'Could not create the offline account. Try again.');
  }
  // An existing account keeps its look unless the player picked one here.
  const account = activeAccount();
  if (account && (lookChosen || !existing)) {
    setSelectedSkin(await selectedSkinFor(look), launcherSkinAccountKey(account.account_id));
  }
}

async function readConfig(): Promise<void> {
  config.value = configResponse(await api('GET', '/config'));
}

async function complete(): Promise<void> {
  let failure: unknown;
  try {
    const error = dtoError(await api('POST', '/onboarding/complete', { expected_revision: config.value?.revision }));
    if (error) failure = new Error(error);
  } catch (err: unknown) {
    failure = err;
  }
  // A lost response may still have committed, so the saved config decides.
  await readConfig();
  if (!config.value?.onboarding_done) throw failure ?? new Error('Onboarding progress was not saved. Try again.');
}

/** Saves the player's choices and marks onboarding done. Rejects with the reason when it is not. */
export async function finishOnboarding(choice: Choice, preferences: Preferences): Promise<void> {
  try {
    const totalGB = Math.floor((systemInfo.value?.total_memory_mb ?? 16 * 1024) / 1024);
    const patch: Record<string, unknown> = {
      // Memory starts at the system recommendation and stays editable in Settings.
      max_memory_mb: Math.round(getMemoryRecommendation(totalGB).rec * 1024),
      music_enabled: preferences.music,
      music_volume: MUSIC_VOLUME,
      telemetry_enabled: preferences.telemetry,
      discord_rpc_enabled: preferences.discord,
      discord_rpc_onboarding_seen: true,
    };
    // Captured before the first await: a later account switch must not redirect the seed.
    const seed = choice.kind === 'microsoft' ? profileSeedQuery() : null;
    if (choice.kind === 'offline') await activateOfflineAccount(choice);
    await saveConfigPatch(patch);
    if (seed) {
      try {
        await api('POST', `/skins/from-profile?${seed.toString()}`, { mark_current: true });
      } catch (err: unknown) {
        console.warn('Could not seed profile skin after onboarding Microsoft sign-in.', err);
      }
    }
    refreshAccountSkin();
    await complete();
  } catch (err: unknown) {
    // Earlier commands may have committed even when a later step failed.
    await readConfig().catch(() => undefined);
    throw err;
  }
  Music.applyConfig({ music_enabled: preferences.music, music_volume: MUSIC_VOLUME });
  if (preferences.music) void Music.play();
}
