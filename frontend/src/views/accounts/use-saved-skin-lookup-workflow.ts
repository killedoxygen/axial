import { useState } from 'preact/hooks';
import { api } from '../../api';
import {
  applySavedSkin,
  endLookupPreview,
  previewLookupSkin,
  refreshWardrobe,
  runWardrobeOp,
  selectSavedSkin,
  setWardrobeNotice,
  wardrobeOp,
} from '../../machines/skin-wardrobe';
import { clampPlayerNameInput } from '../../player-name';
import { toast } from '../../toast';
import { validateUsername } from '../../utils';
import { lookupMinecraftSkin, savedSkinApplyErrorMessage, savedSkinRecord, skinActionErrorMessage } from './api';
import type { MinecraftSkinLookup, SkinVariant } from './types';

export function useSavedSkinLookupWorkflow() {
  const [lookupUsername, setLookupUsername] = useState('');
  const [lookupProfile, setLookupProfile] = useState<MinecraftSkinLookup | null>(null);
  const [lookupState, setLookupState] = useState<'idle' | 'loading' | 'ready' | 'error'>('idle');
  const [lookupError, setLookupError] = useState<string | null>(null);
  const [lookupVariant, setLookupVariant] = useState<SkinVariant>('classic');

  const lookupBusy = wardrobeOp.value?.kind === 'lookup';
  const trimmedLookupUsername = lookupUsername.trim();
  const lookupUsernameError = trimmedLookupUsername ? validateUsername(trimmedLookupUsername) : null;
  const canLookupSkin = Boolean(trimmedLookupUsername) && !lookupUsernameError && wardrobeOp.value === null;
  const canSaveLookupSkin = Boolean(lookupProfile) && lookupState === 'ready' && wardrobeOp.value === null;

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

    await runWardrobeOp({ kind: 'lookup' }, async () => {
      setLookupState('loading');
      setLookupError(null);
      setLookupProfile(null);
      setWardrobeNotice(null);
      try {
        const profile = await lookupMinecraftSkin(trimmedLookupUsername);
        setLookupProfile(profile);
        setLookupState('ready');
        setLookupVariant(profile.variant);
        previewLookupSkin();
      } catch (err) {
        setLookupState('error');
        setLookupError(skinActionErrorMessage(err, 'Could not find that player skin.'));
      }
    });
  };

  const resetLookupForm = (): void => {
    setLookupProfile(null);
    setLookupState('idle');
    setLookupError(null);
    setLookupUsername('');
    setLookupVariant('classic');
  };

  const dismissLookup = (): void => {
    resetLookupForm();
    endLookupPreview();
  };

  const saveUsernameSkin = async (applyAfterSave: boolean): Promise<void> => {
    if (!lookupProfile) {
      setWardrobeNotice('Search for a Minecraft profile before saving this skin.');
      return;
    }

    await runWardrobeOp({ kind: 'lookup' }, async () => {
      setWardrobeNotice(null);
      try {
        const request: { username: string; variant?: SkinVariant } = {
          username: lookupProfile.username,
          variant: lookupVariant,
        };
        const payload = await api('POST', '/skins/from-username', request);
        const saved = savedSkinRecord(payload);
        resetLookupForm();
        endLookupPreview();
        if (saved) selectSavedSkin(saved.texture_key);
        if (saved && applyAfterSave) {
          try {
            toast(await applySavedSkin(saved.texture_key));
          } catch (err) {
            void refreshWardrobe();
            setWardrobeNotice(savedSkinApplyErrorMessage(err));
          }
        } else {
          void refreshWardrobe();
          toast(`${request.username}'s skin added to your library`);
        }
      } catch (err) {
        setWardrobeNotice(skinActionErrorMessage(err, 'Could not save player skin.'));
      }
    });
  };

  const handleLookupUsernameChange = (value: string): void => {
    setLookupUsername(clampPlayerNameInput(value));
    setLookupVariant('classic');
    setLookupProfile(null);
    setLookupState('idle');
    setLookupError(null);
    setWardrobeNotice(null);
    endLookupPreview();
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
