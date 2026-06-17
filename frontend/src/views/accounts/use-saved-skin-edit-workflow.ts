import { useEffect, useRef, useState } from 'preact/hooks';
import { api } from '../../api';
import { pickNativeSkinFile } from '../../native';
import { setSelectedSkin } from '../../player-skin';
import { toast } from '../../toast';
import { showConfirm } from '../../ui/Dialog';
import {
  boundedMessage,
  detectSkinVariantFromSavedSkin,
  isPngFile,
  normalizeSkinUpload,
  replaceSavedSkinTexture,
  savedSkinRecord,
  skinActionErrorMessage,
} from './api';
import type { SavedSkinLibraryMessage } from './SavedSkinLookupBar';
import { useSavedSkinEditReplacementDrop } from './use-saved-skin-edit-replacement-drop';
import { NO_CAPE_VALUE, type SavedSkinRecord, type SkinVariant, type StagedSkinUpload } from './types';

type Setter<T> = (value: T | ((current: T) => T)) => void;

export function useSavedSkinEditWorkflow({
  skins,
  skinActionsEnabled,
  skinAccountKey,
  setMessage,
  setSelectedKey,
  refresh,
  applySavedSkin,
}: {
  skins: SavedSkinRecord[];
  skinActionsEnabled: boolean;
  skinAccountKey: string;
  setMessage: Setter<SavedSkinLibraryMessage | null>;
  setSelectedKey: Setter<string | null>;
  refresh: () => void;
  applySavedSkin: (textureKey: string) => Promise<string>;
}) {
  const editTextureInputRef = useRef<HTMLInputElement | null>(null);
  const editReplacementUrlRef = useRef<string | null>(null);
  const editReplacementTokenRef = useRef(0);
  const editDetectTokenRef = useRef(0);
  const [editReplacement, setEditReplacement] = useState<StagedSkinUpload | null>(null);
  const [editReplacementDragActive, setEditReplacementDragActive] = useState(false);
  const [editKey, setEditKey] = useState<string | null>(null);
  const [editName, setEditName] = useState('');
  const [editVariant, setEditVariant] = useState<SkinVariant>('classic');
  const [editCapeId, setEditCapeId] = useState<string>(NO_CAPE_VALUE);
  const [editBusyKey, setEditBusyKey] = useState<string | null>(null);
  const [editDetectBusyKey, setEditDetectBusyKey] = useState<string | null>(null);
  const [editDetectError, setEditDetectError] = useState<string | null>(null);

  const trimmedEditName = editName.trim();
  const editReplacementReady = !editReplacement || editReplacement.normalizeStatus === 'ready';

  const clearEditReplacement = (): void => {
    editReplacementTokenRef.current += 1;
    if (editReplacementUrlRef.current) {
      URL.revokeObjectURL(editReplacementUrlRef.current);
      editReplacementUrlRef.current = null;
    }
    setEditReplacement(null);
    setEditReplacementDragActive(false);
    if (editTextureInputRef.current) editTextureInputRef.current.value = '';
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

  const startEdit = async (skin: SavedSkinRecord): Promise<void> => {
    if (editKey === skin.texture_key) return;
    const ok = await closeSkinEditBeforeChanging();
    if (!ok) return;

    editDetectTokenRef.current += 1;
    clearEditReplacement();
    setEditKey(skin.texture_key);
    setEditName(skin.name);
    setEditVariant(skin.variant === 'slim' ? 'slim' : 'classic');
    setEditCapeId(skin.cape_id ?? NO_CAPE_VALUE);
    setEditDetectBusyKey(null);
    setEditDetectError(null);
    setMessage(null);
  };

  const cancelEdit = (): void => {
    resetSkinEditState();
  };

  const detectSavedSkinModel = async (skin: SavedSkinRecord): Promise<void> => {
    const token = editDetectTokenRef.current + 1;
    editDetectTokenRef.current = token;
    setEditDetectBusyKey(skin.texture_key);
    setEditDetectError(null);
    setMessage(null);
    try {
      const detectedVariant = await detectSkinVariantFromSavedSkin(skin);
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
      setMessage({ tone: 'err', text: 'Choose a PNG skin file.' });
      if (editTextureInputRef.current) editTextureInputRef.current.value = '';
      return;
    }
    if (editBusyKey) {
      setMessage({ tone: 'err', text: 'Wait for the current skin edit to finish.' });
      if (editTextureInputRef.current) editTextureInputRef.current.value = '';
      return;
    }

    const objectUrl = URL.createObjectURL(file);
    editReplacementTokenRef.current += 1;
    const token = editReplacementTokenRef.current;
    if (editReplacementUrlRef.current) URL.revokeObjectURL(editReplacementUrlRef.current);
    editReplacementUrlRef.current = objectUrl;
    setMessage(null);
    setEditDetectError(null);
    setEditReplacement({
      file,
      objectUrl,
      detectedVariant: 'classic',
      detectingVariant: true,
      normalizeStatus: 'checking',
      applyAfterSave: false,
    });

    void normalizeSkinUpload(file)
      .then((metadata) => {
        if (token !== editReplacementTokenRef.current) return;
        setEditVariant(metadata.variantSuggestion);
        setEditReplacement((current) =>
          current?.objectUrl === objectUrl
            ? {
                ...current,
                detectedVariant: metadata.variantSuggestion,
                detectingVariant: false,
                normalizeStatus: 'ready',
                normalizeError: undefined,
                textureKey: metadata.textureKey,
                originalWidth: metadata.originalWidth,
                originalHeight: metadata.originalHeight,
                normalizedByteSize: metadata.normalizedByteSize,
                normalizedDataUrl: metadata.normalizedDataUrl,
              }
            : current,
        );
      })
      .catch((err) => {
        if (token !== editReplacementTokenRef.current) return;
        setEditReplacement((current) =>
          current?.objectUrl === objectUrl
            ? {
                ...current,
                detectingVariant: false,
                normalizeStatus: 'error',
                normalizeError: boundedMessage(
                  err instanceof Error ? err.message : undefined,
                  'Skin validation failed.',
                ),
              }
            : current,
        );
      });
  };

  const editReplacementDrop = useSavedSkinEditReplacementDrop({
    busy: Boolean(editBusyKey),
    setEditReplacementDragActive,
    setMessage,
    stageEditReplacementFile,
  });

  const openEditTexturePicker = (): void => {
    if (editTextureInputRef.current) editTextureInputRef.current.value = '';
    void (async () => {
      try {
        const nativeFile = await pickNativeSkinFile();
        if (nativeFile) {
          stageEditReplacementFile(nativeFile);
          return;
        }
        if (nativeFile === null) return;
        editTextureInputRef.current?.click();
      } catch (err) {
        setMessage({
          tone: 'err',
          text: boundedMessage(err instanceof Error ? err.message : undefined, 'Could not open skin file.'),
        });
      }
    })();
  };

  const saveSkinMetadata = async (textureKey: string, applyAfterSave = false): Promise<void> => {
    const skin = skins.find((candidate) => candidate.texture_key === textureKey);
    if (skin && !savedSkinEditHasChanges(skin)) {
      setMessage({ tone: 'err', text: 'Make an edit to the skin before saving.' });
      return;
    }
    if (!trimmedEditName) {
      setMessage({ tone: 'err', text: 'Name the skin before saving.' });
      return;
    }
    if (editReplacement && editReplacement.normalizeStatus !== 'ready') {
      setMessage({ tone: 'err', text: 'Wait for the replacement PNG to validate before saving.' });
      return;
    }

    setEditBusyKey(textureKey);
    setMessage(null);
    try {
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
        setSelectedKey(saved.texture_key);
        setSelectedSkin(`saved:${saved.texture_key}`, skinAccountKey);
      } else {
        const payload: { name: string; variant: SkinVariant; cape_id?: string | null } = {
          name: trimmedEditName,
          variant: editVariant,
        };
        if (skin && editCapeId !== (skin.cape_id ?? NO_CAPE_VALUE)) {
          payload.cape_id = editCapeId === NO_CAPE_VALUE ? null : editCapeId;
        }
        const updated = savedSkinRecord(await api('PUT', `/skins/${textureKey}`, payload));
        if (!updated) throw new Error('Skin details update returned an invalid response.');
        savedTextureKey = updated.texture_key;
      }
      cancelEdit();
      if (shouldApplyEditedSkin) {
        try {
          toast(await applySavedSkin(savedTextureKey));
        } catch (err) {
          refresh();
          setMessage({
            tone: 'err',
            text: skinActionErrorMessage(err, 'Minecraft profile apply failed.'),
          });
        }
      } else {
        refresh();
        toast(savedMessage);
      }
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not update skin details.'),
      });
    } finally {
      setEditBusyKey(null);
    }
  };

  useEffect(() => {
    return () => {
      editReplacementTokenRef.current += 1;
      if (editReplacementUrlRef.current) {
        URL.revokeObjectURL(editReplacementUrlRef.current);
        editReplacementUrlRef.current = null;
      }
    };
  }, []);

  return {
    editTextureInputRef,
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
