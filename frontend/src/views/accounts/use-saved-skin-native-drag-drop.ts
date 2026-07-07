import type { RefObject } from 'preact';
import { useEffect, useRef } from 'preact/hooks';
import { onNativeDragDrop, readNativeSkinFile, type NativeDragDropPayload } from '../../native';
import { isPngPath, nativeDragPositionHitsElement, nativeDragTargetElement } from './api';

export function useSavedSkinNativeDragDrop({
  dropSurfaceRef,
  editKey,
  uploadBusy,
  editBusy,
  setUploadDragActive,
  setEditReplacementDragActive,
  notifyError,
  onReadError,
  stageUploadFile,
  stageEditReplacementFile,
}: {
  dropSurfaceRef: RefObject<HTMLElement | null>;
  editKey: string | null;
  uploadBusy: boolean;
  editBusy: boolean;
  setUploadDragActive: (active: boolean) => void;
  setEditReplacementDragActive: (active: boolean) => void;
  notifyError: (text: string) => void;
  onReadError: (error: unknown) => void;
  stageUploadFile: (file: File, applyAfterSave: boolean) => void;
  stageEditReplacementFile: (file: File) => void;
}): void {
  const draggedSkinPathsRef = useRef<string[]>([]);
  const editKeyRef = useRef<string | null>(editKey);
  const uploadBusyRef = useRef(uploadBusy);
  const editBusyRef = useRef(editBusy);
  const readErrorRef = useRef(onReadError);
  const stageUploadFileRef = useRef(stageUploadFile);
  const stageEditReplacementFileRef = useRef(stageEditReplacementFile);

  useEffect(() => {
    editKeyRef.current = editKey;
    uploadBusyRef.current = uploadBusy;
    editBusyRef.current = editBusy;
    readErrorRef.current = onReadError;
    stageUploadFileRef.current = stageUploadFile;
    stageEditReplacementFileRef.current = stageEditReplacementFile;
  });

  useEffect(() => {
    let active = true;
    let subscription: { close(): void } | null = null;

    const pathsForPayload = (payload: NativeDragDropPayload): string[] =>
      payload.paths.length > 0 ? payload.paths : draggedSkinPathsRef.current;

    const editDropTextureKeyForPayload = (payload: NativeDragDropPayload): string | null => {
      const target = nativeDragTargetElement<HTMLElement>(payload.position, '[data-saved-skin-edit-drop-surface]');
      const textureKey = target?.getAttribute('data-saved-skin-edit-drop-surface') ?? null;
      return textureKey && textureKey === editKeyRef.current ? textureKey : null;
    };

    const handleNativeDragDrop = (payload: NativeDragDropPayload): void => {
      if (!active) return;
      if (payload.type === 'leave') {
        draggedSkinPathsRef.current = [];
        setUploadDragActive(false);
        setEditReplacementDragActive(false);
        return;
      }

      const paths = pathsForPayload(payload);
      const skinPaths = paths.filter(isPngPath);
      const editDropTextureKey = editDropTextureKeyForPayload(payload);
      const overDropSurface = nativeDragPositionHitsElement(payload.position, dropSurfaceRef.current);
      const overEditDropSurface = Boolean(editDropTextureKey);

      if (payload.type === 'enter') {
        draggedSkinPathsRef.current = payload.paths;
        setEditReplacementDragActive(skinPaths.length > 0 && overEditDropSurface);
        setUploadDragActive(skinPaths.length > 0 && !overEditDropSurface && overDropSurface);
        return;
      }

      if (payload.type === 'over') {
        setEditReplacementDragActive(skinPaths.length > 0 && overEditDropSurface);
        setUploadDragActive(skinPaths.length > 0 && !overEditDropSurface && overDropSurface);
        return;
      }

      draggedSkinPathsRef.current = [];
      setUploadDragActive(false);
      setEditReplacementDragActive(false);
      if (payload.type !== 'drop' || (!overDropSurface && !overEditDropSurface)) return;
      if (skinPaths.length === 0) return;
      if (skinPaths.length !== 1) {
        notifyError(
          overEditDropSurface ? 'Drop one PNG skin file to replace this texture.' : 'Drop one PNG skin file.',
        );
        return;
      }
      if (overEditDropSurface) {
        if (editBusyRef.current) {
          notifyError('Wait for the current skin edit to finish.');
          return;
        }

        void (async () => {
          try {
            const file = await readNativeSkinFile(skinPaths[0]);
            if (!active || !file) return;
            stageEditReplacementFileRef.current(file);
          } catch (err) {
            if (!active) return;
            readErrorRef.current(err);
          }
        })();
        return;
      }
      if (uploadBusyRef.current) {
        notifyError('Wait for the current skin action to finish.');
        return;
      }

      void (async () => {
        try {
          const file = await readNativeSkinFile(skinPaths[0]);
          if (!active || !file) return;
          stageUploadFileRef.current(file, false);
        } catch (err) {
          if (!active) return;
          readErrorRef.current(err);
        }
      })();
    };

    void onNativeDragDrop(handleNativeDragDrop).then((nextSubscription) => {
      if (!active) {
        nextSubscription?.close();
        return;
      }
      subscription = nextSubscription;
    });

    return () => {
      active = false;
      draggedSkinPathsRef.current = [];
      setUploadDragActive(false);
      setEditReplacementDragActive(false);
      subscription?.close();
    };
  }, [dropSurfaceRef, setEditReplacementDragActive, notifyError, setUploadDragActive]);
}
