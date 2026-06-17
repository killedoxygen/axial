import { useEffect, useRef, useState } from 'preact/hooks';
import { apiUrl } from '../../api';
import { pickNativeSkinFile } from '../../native';
import { setSelectedSkin } from '../../player-skin';
import { toast } from '../../toast';
import {
  apiResponseError,
  boundedMessage,
  isPngFile,
  normalizeSkinUpload,
  resolveUploadSkinVariant,
  savedSkinApplyErrorMessage,
  savedSkinRecord,
  skinActionErrorMessage,
  stagedSkinVariant,
  uploadSkinName,
} from './api';
import type { SavedSkinLibraryMessage } from './SavedSkinLookupBar';
import { useSavedSkinUploadDrop } from './use-saved-skin-upload-drop';
import { NO_CAPE_VALUE, type SkinVariant, type StagedSkinUpload, type UploadSkinVariant } from './types';

type Setter<T> = (value: T | ((current: T) => T)) => void;
type StagePreviewExtra = { kind: 'default'; id: string } | { kind: 'profile' } | { kind: 'lookup' };

export function useSavedSkinUploadWorkflow({
  skinActionsEnabled,
  profileBusy,
  profileResetBusy,
  profileCapeResetBusy,
  lookupBusy,
  skinAccountKey,
  setMessage,
  setSelectedKey,
  setPreviewExtra,
  refresh,
  applySavedSkin,
}: {
  skinActionsEnabled: boolean;
  profileBusy: boolean;
  profileResetBusy: boolean;
  profileCapeResetBusy: boolean;
  lookupBusy: boolean;
  skinAccountKey: string;
  setMessage: Setter<SavedSkinLibraryMessage | null>;
  setSelectedKey: Setter<string | null>;
  setPreviewExtra: Setter<StagePreviewExtra | null>;
  refresh: () => void;
  applySavedSkin: (textureKey: string) => Promise<string>;
}) {
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const uploadApplyAfterSaveRef = useRef(false);
  const stagedUploadUrlRef = useRef<string | null>(null);
  const stagedUploadTokenRef = useRef(0);
  const [skinName, setSkinName] = useState('');
  const [uploadVariant, setUploadVariant] = useState<UploadSkinVariant>('auto');
  const [stagedCapeId, setStagedCapeId] = useState<string>(NO_CAPE_VALUE);
  const [stagedUpload, setStagedUpload] = useState<StagedSkinUpload | null>(null);
  const [busy, setBusy] = useState(false);
  const [uploadDragActive, setUploadDragActive] = useState(false);

  const trimmedName = skinName.trim();
  const canUpload = !busy && !profileBusy && !profileResetBusy && !profileCapeResetBusy && !lookupBusy;
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
    uploadApplyAfterSaveRef.current = false;
    if (fileInputRef.current) fileInputRef.current.value = '';
  };

  const upload = async (
    file: File,
    applyAfterSave = false,
    variantOverride?: SkinVariant,
    capeIdOverride = NO_CAPE_VALUE,
    sourceOverride?: string,
  ): Promise<void> => {
    const name = trimmedName || uploadSkinName(file);
    if (!name) {
      setMessage({ tone: 'err', text: 'Name the skin before uploading.' });
      return;
    }

    setBusy(true);
    setMessage(null);
    try {
      const resolvedVariant = variantOverride ?? (await resolveUploadSkinVariant(file, uploadVariant));
      const params = new URLSearchParams({ name, variant: resolvedVariant });
      if (capeIdOverride !== NO_CAPE_VALUE) params.set('cape_id', capeIdOverride);
      if (sourceOverride) params.set('source', sourceOverride);
      const response = await fetch(apiUrl(`/skins?${params.toString()}`), {
        method: 'POST',
        headers: { 'Content-Type': 'image/png' },
        body: file,
      });
      const payload = await response.json().catch(() => undefined);
      if (!response.ok) {
        throw apiResponseError(response, payload, `Upload failed with HTTP ${response.status}`);
      }
      const saved = savedSkinRecord(payload);
      setSkinName('');
      clearStagedUpload();
      if (saved) {
        setSelectedKey(saved.texture_key);
        setPreviewExtra(null);
        setSelectedSkin(`saved:${saved.texture_key}`, skinAccountKey);
      }
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
        toast('Skin added to your library');
      }
    } catch (err) {
      setMessage({
        tone: 'err',
        text: skinActionErrorMessage(err, 'Could not save skin.'),
      });
    } finally {
      setBusy(false);
      if (fileInputRef.current) fileInputRef.current.value = '';
    }
  };

  const stageUploadFile = (file: File, applyAfterSave: boolean): void => {
    if (!isPngFile(file)) {
      setMessage({ tone: 'err', text: 'Upload a PNG skin file.' });
      if (fileInputRef.current) fileInputRef.current.value = '';
      return;
    }
    if (busy || profileBusy || lookupBusy) {
      setMessage({ tone: 'err', text: 'Wait for the current skin action to finish.' });
      if (fileInputRef.current) fileInputRef.current.value = '';
      return;
    }

    const objectUrl = URL.createObjectURL(file);
    stagedUploadTokenRef.current += 1;
    const token = stagedUploadTokenRef.current;
    if (stagedUploadUrlRef.current) URL.revokeObjectURL(stagedUploadUrlRef.current);
    stagedUploadUrlRef.current = objectUrl;
    setMessage(null);
    setStagedCapeId(NO_CAPE_VALUE);
    setStagedUpload({
      file,
      objectUrl,
      detectedVariant: 'classic',
      detectingVariant: true,
      normalizeStatus: 'checking',
      applyAfterSave,
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
    busy: busy || profileBusy || lookupBusy,
    setUploadDragActive,
    setMessage,
    stageUploadFile,
  });

  const saveStagedUpload = (applyAfterSave: boolean): void => {
    if (!stagedUpload || !stagedVariant || !stagedCanSave) return;
    if (applyAfterSave && !skinActionsEnabled) return;
    void upload(stagedUpload.file, applyAfterSave, stagedVariant, stagedCapeId);
  };

  const handleUploadFile = (file: File, applyAfterSave: boolean): void => {
    stageUploadFile(file, applyAfterSave);
  };

  const handleUploadInputFile = (file: File): void => {
    handleUploadFile(file, uploadApplyAfterSaveRef.current);
    uploadApplyAfterSaveRef.current = false;
  };

  const openUploadPicker = (applyAfterSave: boolean): void => {
    uploadApplyAfterSaveRef.current = applyAfterSave;
    if (fileInputRef.current) fileInputRef.current.value = '';
    void (async () => {
      try {
        const nativeFile = await pickNativeSkinFile();
        if (nativeFile) {
          handleUploadFile(nativeFile, applyAfterSave);
          uploadApplyAfterSaveRef.current = false;
          return;
        }
        if (nativeFile === null) {
          uploadApplyAfterSaveRef.current = false;
          return;
        }
        fileInputRef.current?.click();
      } catch (err) {
        uploadApplyAfterSaveRef.current = false;
        setMessage({
          tone: 'err',
          text: boundedMessage(err instanceof Error ? err.message : undefined, 'Could not open skin file.'),
        });
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
    setUploadBusy: setBusy,
    clearStagedUpload,
    stageUploadFile,
    openUploadPicker,
    saveStagedUpload,
    handleUploadInputFile,
    upload,
  };
}
