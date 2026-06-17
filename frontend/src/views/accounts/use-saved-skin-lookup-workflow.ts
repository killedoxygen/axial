import { useState } from 'preact/hooks';
import { api } from '../../api';
import { clampPlayerNameInput } from '../../player-name';
import { setSelectedSkin } from '../../player-skin';
import { toast } from '../../toast';
import { validateUsername } from '../../utils';
import { lookupMinecraftSkin, savedSkinApplyErrorMessage, savedSkinRecord, skinActionErrorMessage } from './api';
import type { SavedSkinLibraryMessage } from './SavedSkinLookupBar';
import type { MinecraftSkinLookup, SkinVariant } from './types';

type Setter<T> = (value: T | ((current: T) => T)) => void;
type StagePreviewExtra = { kind: 'default'; id: string } | { kind: 'profile' } | { kind: 'lookup' };

export function useSavedSkinLookupWorkflow({
  skinAccountKey,
  setMessage,
  setSelectedKey,
  setPreviewExtra,
  refresh,
  applySavedSkin,
}: {
  skinAccountKey: string;
  setMessage: Setter<SavedSkinLibraryMessage | null>;
  setSelectedKey: Setter<string | null>;
  setPreviewExtra: Setter<StagePreviewExtra | null>;
  refresh: () => void;
  applySavedSkin: (textureKey: string) => Promise<string>;
}) {
  const [lookupUsername, setLookupUsername] = useState('');
  const [lookupProfile, setLookupProfile] = useState<MinecraftSkinLookup | null>(null);
  const [lookupState, setLookupState] = useState<'idle' | 'loading' | 'ready' | 'error'>('idle');
  const [lookupError, setLookupError] = useState<string | null>(null);
  const [lookupVariant, setLookupVariant] = useState<SkinVariant>('classic');
  const [lookupBusy, setLookupBusy] = useState(false);

  const trimmedLookupUsername = lookupUsername.trim();
  const lookupUsernameError = trimmedLookupUsername ? validateUsername(trimmedLookupUsername) : null;
  const canLookupSkin = Boolean(trimmedLookupUsername) && !lookupUsernameError && !lookupBusy;
  const canSaveLookupSkin = Boolean(lookupProfile) && lookupState === 'ready' && !lookupBusy;

  const lookupSkin = async (): Promise<void> => {
    if (!trimmedLookupUsername) {
      setLookupState('error');
      setLookupError('Enter a Minecraft username.');
      return;
    }
    if (lookupUsernameError) {
      setLookupState('error');
      setLookupError(lookupUsernameError);
      return;
    }

    setLookupBusy(true);
    setLookupState('loading');
    setLookupError(null);
    setLookupProfile(null);
    setMessage(null);
    try {
      const profile = await lookupMinecraftSkin(trimmedLookupUsername);
      setLookupProfile(profile);
      setLookupState('ready');
      setLookupVariant(profile.variant);
      setPreviewExtra({ kind: 'lookup' });
    } catch (err) {
      setLookupState('error');
      setLookupError(skinActionErrorMessage(err, 'Could not find that player skin.'));
    } finally {
      setLookupBusy(false);
    }
  };

  const dismissLookup = (): void => {
    setLookupProfile(null);
    setLookupState('idle');
    setLookupError(null);
    setLookupUsername('');
    setPreviewExtra((current) => (current?.kind === 'lookup' ? null : current));
  };

  const saveUsernameSkin = async (applyAfterSave: boolean): Promise<void> => {
    if (!lookupProfile) {
      setMessage({ tone: 'err', text: 'Search for a Minecraft profile before saving this skin.' });
      return;
    }

    setLookupBusy(true);
    setMessage(null);
    try {
      const request: { username: string; variant?: SkinVariant } = {
        username: lookupProfile.username,
        variant: lookupVariant,
      };
      const payload = await api('POST', '/skins/from-username', request);
      const saved = savedSkinRecord(payload);
      if (saved) {
        setSelectedKey(saved.texture_key);
        setSelectedSkin(`saved:${saved.texture_key}`, skinAccountKey);
      }
      setLookupUsername('');
      setLookupVariant('classic');
      setLookupProfile(null);
      setLookupState('idle');
      setLookupError(null);
      setPreviewExtra(null);
      if (saved && applyAfterSave) {
        try {
          toast(await applySavedSkin(saved.texture_key));
        } catch (err) {
          setSelectedKey(saved.texture_key);
          refresh();
          setMessage({ tone: 'err', text: savedSkinApplyErrorMessage(err) });
        }
      } else {
        refresh();
        toast(`${request.username}'s skin added to your library`);
      }
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not save player skin.'),
      });
    } finally {
      setLookupBusy(false);
    }
  };

  const handleLookupUsernameChange = (value: string): void => {
    setLookupUsername(clampPlayerNameInput(value));
    setLookupVariant('classic');
    setLookupProfile(null);
    setLookupState('idle');
    setLookupError(null);
    setMessage(null);
    setPreviewExtra((current) => (current?.kind === 'lookup' ? null : current));
  };

  return {
    lookupUsername,
    lookupProfile,
    lookupState,
    lookupError,
    lookupVariant,
    lookupBusy,
    lookupUsernameError,
    canLookupSkin,
    canSaveLookupSkin,
    lookupSkin,
    dismissLookup,
    saveUsernameSkin,
    handleLookupUsernameChange,
  };
}
