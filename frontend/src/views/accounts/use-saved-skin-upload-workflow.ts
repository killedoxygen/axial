import { useEffect, useRef, useState } from 'preact/hooks';
import {
  runWardrobeOp,
  setWardrobeNotice,
  uploadSkinPng,
  wardrobeBusy,
  wardrobeOp,
} from '../../machines/skin-wardrobe';
import { pickNativeSkinFile } from '../../native';
import {
  boundedMessage,
  isPngFile,
  normalizeSkinUpload,
  resolveUploadSkinVariant,
  skinActionErrorMessage,
  stagedSkinVariant,
  uploadSkinName,
} from './api';
import { useSavedSkinUploadDrop } from './use-saved-skin-upload-drop';
import { NO_CAPE_VALUE, type SkinVariant, type StagedSkinUpload, type UploadSkinVariant } from './types';

export function useSavedSkinUploadWorkflow() {
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const stagedUploadUrlRef = useRef<string | null>(null);
  const stagedUploadTokenRef = useRef(0);
  const [skinName, setSkinName] = useState('');
  const [uploadVariant, setUploadVariant] = useState<UploadSkinVariant>('auto');
  const [stagedCapeId, setStagedCapeId] = useState<string>(NO_CAPE_VALUE);
  const [stagedUpload, setStagedUpload] = useState<StagedSkinUpload | null>(null);
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
    stagedUploadTokenRef.current += 1;
    if (stagedUploadUrlRef.current) {
      URL.revokeObjectURL(stagedUploadUrlRef.current);
      stagedUploadUrlRef.current = null;
    }
    setStagedUpload(null);
    setStagedCapeId(NO_CAPE_VALUE);
    if (fileInputRef.current) fileInputRef.current.value = '';
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
      setWardrobeNotice(null);
      try {
        const resolvedVariant = variantOverride ?? (await resolveUploadSkinVariant(file, uploadVariant));
        await uploadSkinPng(file, {
          name,
          variant: resolvedVariant,
          capeId: capeIdOverride === NO_CAPE_VALUE ? undefined : capeIdOverride,
          applyAfterSave,
        });
        setSkinName('');
        clearStagedUpload();
      } catch (err) {
        setWardrobeNotice(skinActionErrorMessage(err, 'Could not save skin.'));
      } finally {
        if (fileInputRef.current) fileInputRef.current.value = '';
      }
    });
  };

  const stageUploadFile = (file: File): void => {
    if (!isPngFile(file)) {
      setWardrobeNotice('Upload a PNG skin file.');
      if (fileInputRef.current) fileInputRef.current.value = '';
      return;
    }
    if (wardrobeBusy()) {
      setWardrobeNotice('Wait for the current skin action to finish.');
      if (fileInputRef.current) fileInputRef.current.value = '';
      return;
    }

    const objectUrl = URL.createObjectURL(file);
    stagedUploadTokenRef.current += 1;
    const token = stagedUploadTokenRef.current;
    if (stagedUploadUrlRef.current) URL.revokeObjectURL(stagedUploadUrlRef.current);
    stagedUploadUrlRef.current = objectUrl;
    setWardrobeNotice(null);
    setStagedCapeId(NO_CAPE_VALUE);
    setStagedUpload({
      file,
      objectUrl,
      detectedVariant: 'classic',
      detectingVariant: true,
      normalizeStatus: 'checking',
    });

    void normalizeSkinUpload(file)
      .then((metadata) => {
        if (token !== stagedUploadTokenRef.current) return;
        setStagedUpload((current) =>
          current?.objectUrl === objectUrl
            ? {
                ...current,
                detectedVariant: metadata.variantSuggestion,
                detectingVariant: false,
                normalizeStatus: 'ready',
                normalizeError: undefined,
                normalizedDataUrl: metadata.normalizedDataUrl,
              }
            : current,
        );
      })
      .catch((err) => {
        if (token !== stagedUploadTokenRef.current) return;
        setStagedUpload((current) =>
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

  const uploadDrop = useSavedSkinUploadDrop({
    busy: wardrobeBusy(),
    setUploadDragActive,
    notifyError: setWardrobeNotice,
    stageUploadFile,
  });

  const saveStagedUpload = (applyAfterSave: boolean): void => {
    if (!stagedUpload || !stagedVariant || !stagedCanSave) return;
    void upload(stagedUpload.file, applyAfterSave, stagedVariant, stagedCapeId);
  };

  const handleUploadInputFile = (file: File): void => {
    stageUploadFile(file);
  };

  const openUploadPicker = (): void => {
    if (fileInputRef.current) fileInputRef.current.value = '';
    void (async () => {
      try {
        const nativeFile = await pickNativeSkinFile();
        if (nativeFile) {
          stageUploadFile(nativeFile);
          return;
        }
        if (nativeFile === null) return;
        fileInputRef.current?.click();
      } catch (err) {
        setWardrobeNotice(boundedMessage(err instanceof Error ? err.message : undefined, 'Could not open skin file.'));
      }
    })();
  };

  useEffect(() => {
    return () => {
      stagedUploadTokenRef.current += 1;
      if (stagedUploadUrlRef.current) {
        URL.revokeObjectURL(stagedUploadUrlRef.current);
        stagedUploadUrlRef.current = null;
      }
    };
  }, []);

  return {
    fileInputRef,
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
