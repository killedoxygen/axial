import { useEffect, useRef, useState } from 'preact/hooks';
import { api } from '../../api';
import {
  applySavedSkin,
  captureWardrobeContext,
  endLookupPreview,
  isWardrobeContextCurrent,
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
  const lookupRequest = useRef(0);

  useEffect(() => () => { lookupRequest.current += 1; }, []);

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
      const requestId = ++lookupRequest.current;
      const capture = captureWardrobeContext();
      setLookupState('loading');
      setLookupError(null);
      setLookupProfile(null);
      setWardrobeNotice(null);
      try {
        const profile = await lookupMinecraftSkin(trimmedLookupUsername);
        if (requestId !== lookupRequest.current || !isWardrobeContextCurrent(capture)) return;
        setLookupProfile(profile);
        setLookupState('ready');
        setLookupVariant(profile.variant);
        previewLookupSkin();
      } catch (err) {
        if (requestId !== lookupRequest.current || !isWardrobeContextCurrent(capture)) return;
        setLookupState('error');
        setLookupError(skinActionErrorMessage(err, 'Could not find that player skin.'));
      }
    });
  };

  const resetLookupForm = (): void => {
    lookupRequest.current += 1;
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
      const capture = captureWardrobeContext();
      setWardrobeNotice(null);
      try {
        const request: { username: string; variant?: SkinVariant } = {
          username: lookupProfile.username,
          variant: lookupVariant,
        };
        const payload = await api('POST', '/skins/from-username', request);
        const saved = savedSkinRecord(payload);
        if (!saved) throw new Error('Player skin save returned an invalid response.');
        if (!isWardrobeContextCurrent(capture)) {
          void refreshWardrobe();
          return;
        }
        resetLookupForm();
        endLookupPreview();
        selectSavedSkin(saved.texture_key);
        if (applyAfterSave) {
          try {
            toast(await applySavedSkin(saved.texture_key, { capture }));
          } catch (err) {
            void refreshWardrobe();
            if (isWardrobeContextCurrent(capture)) setWardrobeNotice(savedSkinApplyErrorMessage(err));
          }
        } else {
          void refreshWardrobe();
          toast(`${request.username}'s skin added to your library`);
        }
      } catch (err) {
        if (isWardrobeContextCurrent(capture)) setWardrobeNotice(skinActionErrorMessage(err, 'Could not save player skin.'));
      }
    });
  };

  const handleLookupUsernameChange = (value: string): void => {
    lookupRequest.current += 1;
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
