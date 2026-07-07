import { useRef } from 'preact/hooks';

export function useSavedSkinUploadDrop({
  busy,
  setUploadDragActive,
  notifyError,
  stageUploadFile,
}: {
  busy: boolean;
  setUploadDragActive: (active: boolean) => void;
  notifyError: (text: string) => void;
  stageUploadFile: (file: File, applyAfterSave: boolean) => void;
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
      setUploadDragActive(false);

      const files = event.dataTransfer?.files;
      if (!files || files.length === 0) return;
      if (files.length !== 1) {
        notifyError('Drop one PNG skin file.');
        return;
      }

      stageUploadFile(files[0], false);
    },
    onDragEnter(event) {
      if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
      event.preventDefault();
      event.stopPropagation();
      dragDepthRef.current += 1;
      setUploadDragActive(true);
    },
    onDragOver(event) {
      if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
      event.preventDefault();
      if (event.dataTransfer) event.dataTransfer.dropEffect = busy ? 'none' : 'copy';
    },
    onDragLeave(event) {
      if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
      event.preventDefault();
      event.stopPropagation();
      dragDepthRef.current = Math.max(0, dragDepthRef.current - 1);
      if (dragDepthRef.current === 0) setUploadDragActive(false);
    },
  };
}
