import { useState } from 'preact/hooks';
import {
  captureWardrobeContext,
  isWardrobeContextCurrent,
  runWardrobeOp,
  setWardrobeNotice,
  uploadSkinPng,
  wardrobeBusy,
  wardrobeOp,
} from './wardrobe-state';
import { isPngFile, skinActionErrorMessage, stagedSkinVariant, uploadSkinName } from './api';
import { usePngDrop } from './use-png-drop';
import { useStagedSkinPng } from './use-staged-skin-png';
import { NO_CAPE_VALUE, type UploadSkinVariant } from './types';
import { type SkinVariant } from '../generated/SkinVariant';

export function useSavedSkinUploadWorkflow() {
  const png = useStagedSkinPng();
  const stagedUpload = png.staged;
  const [skinName, setSkinName] = useState('');
  const [uploadVariant, setUploadVariant] = useState<UploadSkinVariant>('auto');
  const [stagedCapeId, setStagedCapeId] = useState<string>(NO_CAPE_VALUE);
  const [uploadDragActive, setUploadDragActive] = useState(false);

  const busy = wardrobeOp.value?.kind === 'upload';
  const trimmedName = skinName.trim();
  const canUpload = !wardrobeBusy();
  const stagedVariant = stagedUpload ? stagedSkinVariant(stagedUpload, uploadVariant) : null;
  const stagedName = stagedUpload ? trimmedName || uploadSkinName(stagedUpload.file) || 'Uploaded skin' : '';
  const stagedVariantReady = Boolean(stagedUpload && (uploadVariant !== 'auto' || !stagedUpload.detectingVariant));
  const stagedValidated = stagedUpload?.normalizeStatus === 'ready';
  const stagedCanSave = Boolean(stagedUpload && canUpload && stagedVariantReady && stagedValidated);

  const clearStagedUpload = (): void => {
    png.clear();
    setStagedCapeId(NO_CAPE_VALUE);
  };

  const upload = async (
    file: File,
    applyAfterSave = false,
    variantOverride?: SkinVariant,
    capeIdOverride = NO_CAPE_VALUE,
  ): Promise<void> => {
    const name = trimmedName || uploadSkinName(file);
    if (!name) {
      setWardrobeNotice('Name the skin before uploading.');
      return;
    }

    await runWardrobeOp({ kind: 'upload' }, async () => {
      const capture = captureWardrobeContext();
      setWardrobeNotice(null);
      try {
        const resolvedVariant = variantOverride ?? (uploadVariant === 'auto' ? undefined : uploadVariant);
        if (!isWardrobeContextCurrent(capture)) return;
        await uploadSkinPng(file, {
          name,
          variant: resolvedVariant,
          capeId: capeIdOverride === NO_CAPE_VALUE ? undefined : capeIdOverride,
          applyAfterSave,
          capture,
        });
        if (!isWardrobeContextCurrent(capture)) return;
        setSkinName('');
        clearStagedUpload();
      } catch (err) {
        if (isWardrobeContextCurrent(capture)) setWardrobeNotice(skinActionErrorMessage(err, 'Could not save skin.'));
      } finally {
        png.resetInput();
      }
    });
  };

  const stageUploadFile = (file: File): void => {
    if (!isPngFile(file)) {
      png.refuse('Upload a PNG skin file.');
      return;
    }
    if (wardrobeBusy()) {
      png.refuse('Wait for the current skin action to finish.');
      return;
    }
    setStagedCapeId(NO_CAPE_VALUE);
    png.stage(file);
  };

  const uploadDrop = usePngDrop({
    busy: wardrobeBusy(),
    setActive: setUploadDragActive,
    notifyError: setWardrobeNotice,
    tooMany: 'Drop one PNG skin file.',
    stage: stageUploadFile,
  });

  const saveStagedUpload = (applyAfterSave: boolean): void => {
    if (!stagedUpload || !stagedVariant || !stagedCanSave) return;
    void upload(stagedUpload.file, applyAfterSave, stagedVariant, stagedCapeId);
  };

  const handleUploadInputFile = (file: File): void => {
    stageUploadFile(file);
  };

  const openUploadPicker = (): void => {
    png.openPicker(stageUploadFile);
  };

  return {
    fileInputRef: png.inputRef,
    stagedUpload,
    stagedVariant,
    stagedName,
    stagedCapeId,
    skinName,
    uploadVariant,
    uploadDragActive,
    busy,
    canUpload,
    stagedCanSave,
    uploadDrop,
    setSkinName,
    setUploadVariant,
    setStagedCapeId,
    setUploadDragActive,
    clearStagedUpload,
    stageUploadFile,
    openUploadPicker,
    saveStagedUpload,
    handleUploadInputFile,
  };
}
