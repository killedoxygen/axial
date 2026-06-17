import { useRef } from 'preact/hooks';
import type { SavedSkinLibraryMessage } from './SavedSkinLookupBar';

export function useSavedSkinEditReplacementDrop({
  busy,
  setEditReplacementDragActive,
  setMessage,
  stageEditReplacementFile,
}: {
  busy: boolean;
  setEditReplacementDragActive: (active: boolean) => void;
  setMessage: (message: SavedSkinLibraryMessage) => void;
  stageEditReplacementFile: (file: File) => void;
}): {
  onDrop: (event: DragEvent) => void;
  onDragEnter: (event: DragEvent) => void;
  onDragOver: (event: DragEvent) => void;
  onDragLeave: (event: DragEvent) => void;
} {
  const dragDepthRef = useRef(0);

  return {
    onDrop(event) {
      event.preventDefault();
      event.stopPropagation();
      dragDepthRef.current = 0;
      setEditReplacementDragActive(false);

      const files = event.dataTransfer?.files;
      if (!files || files.length === 0) return;
      if (files.length !== 1) {
        setMessage({ tone: 'err', text: 'Drop one PNG skin file to replace this texture.' });
        return;
      }

      stageEditReplacementFile(files[0]);
    },
    onDragEnter(event) {
      if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
      event.preventDefault();
      event.stopPropagation();
      dragDepthRef.current += 1;
      setEditReplacementDragActive(true);
    },
    onDragOver(event) {
      if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
      event.preventDefault();
      event.stopPropagation();
      if (event.dataTransfer) event.dataTransfer.dropEffect = busy ? 'none' : 'copy';
    },
    onDragLeave(event) {
      if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
      event.preventDefault();
      event.stopPropagation();
      dragDepthRef.current = Math.max(0, dragDepthRef.current - 1);
      if (dragDepthRef.current === 0) setEditReplacementDragActive(false);
    },
  };
}
