import type { RefObject } from 'preact';
import { useEffect, useRef } from 'preact/hooks';
import { consumeNativeSkinDrop, onNativeDragDrop, type NativeDragDropPayload } from '../../native';
import { nativeDragPositionHitsElement, nativeDragTargetElement } from './api';

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
  stageUploadFile: (file: File) => void;
  stageEditReplacementFile: (file: File) => void;
}): void {
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

    const editDropTextureKeyForPayload = (payload: NativeDragDropPayload): string | null => {
      const target = nativeDragTargetElement<HTMLElement>(payload.position, '[data-saved-skin-edit-drop-surface]');
      const textureKey = target?.getAttribute('data-saved-skin-edit-drop-surface') ?? null;
      return textureKey && textureKey === editKeyRef.current ? textureKey : null;
    };

    const handleNativeDragDrop = (payload: NativeDragDropPayload): void => {
      if (!active) return;
      if (payload.type === 'leave') {
        setUploadDragActive(false);
        setEditReplacementDragActive(false);
        return;
      }

      const editDropTextureKey = editDropTextureKeyForPayload(payload);
      const overDropSurface = nativeDragPositionHitsElement(payload.position, dropSurfaceRef.current);
      const overEditDropSurface = Boolean(editDropTextureKey);

      if (payload.type === 'enter') {
        setEditReplacementDragActive(payload.eligible && overEditDropSurface);
        setUploadDragActive(payload.eligible && !overEditDropSurface && overDropSurface);
        return;
      }

      if (payload.type === 'over') {
        setEditReplacementDragActive(payload.eligible && overEditDropSurface);
        setUploadDragActive(payload.eligible && !overEditDropSurface && overDropSurface);
        return;
      }

      setUploadDragActive(false);
      setEditReplacementDragActive(false);
      if (payload.type !== 'drop' || (!overDropSurface && !overEditDropSurface)) return;
      if (payload.error) {
        notifyError(payload.error);
        return;
      }
      if (!payload.token) return;
      const token = payload.token;
      if (overEditDropSurface) {
        if (editBusyRef.current) {
          notifyError('Wait for the current skin edit to finish.');
          return;
        }

        void (async () => {
          try {
            const file = await consumeNativeSkinDrop(token);
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
          const file = await consumeNativeSkinDrop(token);
          if (!active || !file) return;
          stageUploadFileRef.current(file);
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
      setUploadDragActive(false);
      setEditReplacementDragActive(false);
      subscription?.close();
    };
  }, [dropSurfaceRef, setEditReplacementDragActive, notifyError, setUploadDragActive]);
}
