import { useEffect, useRef, useState } from 'preact/hooks';
import { setWardrobeNotice } from './wardrobe-state';
import { pickNativeSkinFile } from '../platform/native';
import { boundedMessage, normalizeSkinUpload } from './api';
import { type StagedSkinUpload } from './types';
import { type SkinVariant } from '../generated/SkinVariant';

/**
 * One staged skin PNG and its file input: owns the preview object URL and invalidates a pending normalization when
 * the file is replaced, cleared or unmounted. Callers admit files and decide what staging means for their draft.
 */
export function useStagedSkinPng({ onDetected }: { onDetected?: (variant: SkinVariant) => void } = {}) {
  const inputRef = useRef<HTMLInputElement | null>(null);
  const urlRef = useRef<string | null>(null);
  const tokenRef = useRef(0);
  const [staged, setStaged] = useState<StagedSkinUpload | null>(null);

  const resetInput = (): void => {
    if (inputRef.current) inputRef.current.value = '';
  };

  const release = (): void => {
    tokenRef.current += 1;
    if (urlRef.current) {
      URL.revokeObjectURL(urlRef.current);
      urlRef.current = null;
    }
  };

  const clear = (): void => {
    release();
    setStaged(null);
    resetInput();
  };

  const refuse = (message: string): void => {
    setWardrobeNotice(message);
    resetInput();
  };

  const stage = (file: File): void => {
    const objectUrl = URL.createObjectURL(file);
    release();
    const token = tokenRef.current;
    urlRef.current = objectUrl;
    setWardrobeNotice(null);
    setStaged({ file, objectUrl, detectedVariant: 'classic', detectingVariant: true, normalizeStatus: 'checking' });

    void normalizeSkinUpload(file)
      .then((metadata) => {
        if (token !== tokenRef.current) return;
        onDetected?.(metadata.variant_suggestion);
        setStaged((current) =>
          current?.objectUrl === objectUrl
            ? {
                ...current,
                detectedVariant: metadata.variant_suggestion,
                detectingVariant: false,
                normalizeStatus: 'ready',
                normalizeError: undefined,
                normalizedDataUrl: metadata.normalized_data_url,
              }
            : current,
        );
      })
      .catch((err) => {
        if (token !== tokenRef.current) return;
        setStaged((current) =>
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

  /** Prefers the native picker, falling back to the file input where none exists. */
  const openPicker = (admit: (file: File) => void): void => {
    resetInput();
    void (async () => {
      try {
        const nativeFile = await pickNativeSkinFile();
        if (nativeFile) {
          admit(nativeFile);
          return;
        }
        if (nativeFile === null) return;
        inputRef.current?.click();
      } catch (err) {
        setWardrobeNotice(boundedMessage(err instanceof Error ? err.message : undefined, 'Could not open skin file.'));
      }
    })();
  };

  useEffect(() => release, []);

  return { inputRef, staged, stage, clear, refuse, resetInput, openPicker };
}
