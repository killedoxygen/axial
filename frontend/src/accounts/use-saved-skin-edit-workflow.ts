import { useRef, useState } from 'preact/hooks';
import {
  applySavedSkin,
  captureWardrobeContext,
  isWardrobeContextCurrent,
  refreshWardrobe,
  runWardrobeOp,
  selectSavedSkin,
  setWardrobeNotice,
  wardrobeContext,
  wardrobeData,
  wardrobeOp,
} from './wardrobe-state';
import { toast } from '../app/toast';
import { showConfirm } from '../ui/Dialog';
import {
  boundedMessage,
  suggestedSavedSkinVariant,
  isPngFile,
  replaceSavedSkinTexture,
  skinActionErrorMessage,
  updateSavedSkin,
} from './api';
import { usePngDrop } from './use-png-drop';
import { useStagedSkinPng } from './use-staged-skin-png';
import { NO_CAPE_VALUE } from './types';
import { type SavedSkinRecord } from '../generated/SavedSkinRecord';
import { type SkinVariant } from '../generated/SkinVariant';

export function useSavedSkinEditWorkflow() {
  const editDetectTokenRef = useRef(0);
  const [editReplacementDragActive, setEditReplacementDragActive] = useState(false);
  const [editKey, setEditKey] = useState<string | null>(null);
  const [editName, setEditName] = useState('');
  const [editVariant, setEditVariant] = useState<SkinVariant>('classic');
  const replacement = useStagedSkinPng({ onDetected: setEditVariant });
  const editReplacement = replacement.staged;
  const [editCapeId, setEditCapeId] = useState<string>(NO_CAPE_VALUE);
  const [editDetectBusyKey, setEditDetectBusyKey] = useState<string | null>(null);
  const [editDetectError, setEditDetectError] = useState<string | null>(null);

  const skins = wardrobeData.value.skins;
  const editBusyKey = wardrobeOp.value?.kind === 'edit' ? (wardrobeOp.value.key ?? null) : null;
  const trimmedEditName = editName.trim();
  const editReplacementReady = !editReplacement || editReplacement.normalizeStatus === 'ready';

  const clearEditReplacement = (): void => {
    replacement.clear();
    setEditReplacementDragActive(false);
  };

  const savedSkinEditHasChanges = (skin: SavedSkinRecord): boolean => {
    const nextCapeId = editCapeId === NO_CAPE_VALUE ? null : editCapeId;
    return (
      Boolean(editReplacement) ||
      trimmedEditName !== skin.name ||
      editVariant !== skin.variant ||
      nextCapeId !== (skin.cape_id ?? null)
    );
  };

  const hasUnsavedSkinEdit = (): boolean => {
    if (!editKey) return false;
    const skin = skins.find((candidate) => candidate.texture_key === editKey);
    if (!skin) return Boolean(editReplacement);
    return savedSkinEditHasChanges(skin);
  };

  const resetSkinEditState = (): void => {
    editDetectTokenRef.current += 1;
    clearEditReplacement();
    setEditKey(null);
    setEditName('');
    setEditVariant('classic');
    setEditCapeId(NO_CAPE_VALUE);
    setEditDetectBusyKey(null);
    setEditDetectError(null);
  };

  const closeSkinEditBeforeChanging = async (): Promise<boolean> => {
    if (!editKey) return true;
    if (hasUnsavedSkinEdit()) {
      const ok = await showConfirm('Discard unsaved skin edits? Your local skin record will stay unchanged.', {
        title: 'Discard skin edits',
        destructive: true,
        confirmText: 'Discard',
      });
      if (!ok) return false;
    }
    resetSkinEditState();
    return true;
  };

  const startEdit = async (skin: SavedSkinRecord): Promise<boolean> => {
    if (editKey === skin.texture_key) return true;
    const ok = await closeSkinEditBeforeChanging();
    if (!ok) return false;

    editDetectTokenRef.current += 1;
    clearEditReplacement();
    setEditKey(skin.texture_key);
    setEditName(skin.name);
    setEditVariant(skin.variant === 'slim' ? 'slim' : 'classic');
    setEditCapeId(skin.cape_id ?? NO_CAPE_VALUE);
    setEditDetectBusyKey(null);
    setEditDetectError(null);
    setWardrobeNotice(null);
    return true;
  };

  const cancelEdit = (): void => {
    resetSkinEditState();
  };

  const detectSavedSkinModel = async (skin: SavedSkinRecord): Promise<void> => {
    const token = editDetectTokenRef.current + 1;
    editDetectTokenRef.current = token;
    setEditDetectBusyKey(skin.texture_key);
    setEditDetectError(null);
    setWardrobeNotice(null);
    try {
      const detectedVariant = await suggestedSavedSkinVariant(skin);
      if (token !== editDetectTokenRef.current) return;
      setEditVariant(detectedVariant);
    } catch (err) {
      if (token !== editDetectTokenRef.current) return;
      setEditDetectError(
        boundedMessage(err instanceof Error ? err.message : undefined, 'Could not detect skin model.'),
      );
    } finally {
      if (token === editDetectTokenRef.current) setEditDetectBusyKey(null);
    }
  };

  const stageEditReplacementFile = (file: File): void => {
    if (!editKey) return;
    if (!isPngFile(file)) {
      replacement.refuse('Choose a PNG skin file.');
      return;
    }
    if (editBusyKey) {
      replacement.refuse('Wait for the current skin edit to finish.');
      return;
    }
    setEditDetectError(null);
    replacement.stage(file);
  };

  const editReplacementDrop = usePngDrop({
    busy: Boolean(editBusyKey),
    setActive: setEditReplacementDragActive,
    notifyError: setWardrobeNotice,
    tooMany: 'Drop one PNG skin file to replace this texture.',
    stage: stageEditReplacementFile,
  });

  const openEditTexturePicker = (): void => {
    replacement.openPicker(stageEditReplacementFile);
  };

  const saveSkinMetadata = async (textureKey: string, applyAfterSave = false): Promise<void> => {
    const skin = skins.find((candidate) => candidate.texture_key === textureKey);
    if (skin && !savedSkinEditHasChanges(skin)) {
      setWardrobeNotice('Make an edit to the skin before saving.');
      return;
    }
    if (!trimmedEditName) {
      setWardrobeNotice('Name the skin before saving.');
      return;
    }
    if (editReplacement && editReplacement.normalizeStatus !== 'ready') {
      setWardrobeNotice('Wait for the replacement PNG to validate before saving.');
      return;
    }

    await runWardrobeOp({ kind: 'edit', key: textureKey }, async () => {
      const capture = captureWardrobeContext();
      setWardrobeNotice(null);
      try {
        const skinActionsEnabled = wardrobeContext.value.skinActionsEnabled;
        const previousCapeId = skin?.cape_id ?? null;
        const nextCapeId = editCapeId === NO_CAPE_VALUE ? null : editCapeId;
        const profileRelevantEdit = Boolean(
          editReplacement || (skin && editVariant !== skin.variant) || nextCapeId !== previousCapeId,
        );
        const shouldReapplyEditedSkin = Boolean(skin?.applied_at && profileRelevantEdit);
        const shouldApplyEditedSkin = Boolean(
          skinActionsEnabled && ((applyAfterSave && !skin?.applied_at) || shouldReapplyEditedSkin),
        );
        let savedTextureKey = textureKey;
        const savedMessage = editReplacement ? 'Skin texture replaced.' : 'Skin details updated.';
        if (editReplacement) {
          const saved = await replaceSavedSkinTexture(textureKey, editReplacement.file, {
            name: trimmedEditName,
            variant: editVariant,
            capeId: nextCapeId === previousCapeId ? undefined : nextCapeId,
          });
          savedTextureKey = saved.texture_key;
          if (isWardrobeContextCurrent(capture)) selectSavedSkin(saved.texture_key);
        } else {
          const payload: { name: string; variant: SkinVariant; cape_id?: string | null } = {
            name: trimmedEditName,
            variant: editVariant,
          };
          if (skin && editCapeId !== (skin.cape_id ?? NO_CAPE_VALUE)) {
            payload.cape_id = editCapeId === NO_CAPE_VALUE ? null : editCapeId;
          }
          const updated = await updateSavedSkin(textureKey, payload);
          savedTextureKey = updated.texture_key;
        }
        if (!isWardrobeContextCurrent(capture)) {
          void refreshWardrobe();
          return;
        }
        cancelEdit();
        if (shouldApplyEditedSkin) {
          try {
            toast(await applySavedSkin(savedTextureKey, { capture }));
          } catch (err) {
            void refreshWardrobe();
            if (isWardrobeContextCurrent(capture))
              setWardrobeNotice(skinActionErrorMessage(err, 'Minecraft profile apply failed.'));
          }
        } else {
          void refreshWardrobe();
          toast(savedMessage);
        }
      } catch (err) {
        if (isWardrobeContextCurrent(capture))
          setWardrobeNotice(skinActionErrorMessage(err, 'Could not update skin details.'));
      }
    });
  };

  return {
    editTextureInputRef: replacement.inputRef,
    editReplacement,
    editReplacementDragActive,
    editKey,
    editName,
    editVariant,
    editCapeId,
    editBusyKey,
    editDetectBusyKey,
    editDetectError,
    trimmedEditName,
    editReplacementReady,
    editReplacementDrop,
    setEditName,
    setEditVariant,
    setEditCapeId,
    setEditDetectError,
    setEditReplacementDragActive,
    clearEditReplacement,
    closeSkinEditBeforeChanging,
    startEdit,
    cancelEdit,
    detectSavedSkinModel,
    stageEditReplacementFile,
    openEditTexturePicker,
    saveSkinMetadata,
    savedSkinEditHasChanges,
  };
}
