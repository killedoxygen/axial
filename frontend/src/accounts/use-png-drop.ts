import { useRef } from 'preact/hooks';

/** Drag handlers for a surface that takes exactly one dropped PNG. */
export function usePngDrop({
  busy,
  setActive,
  notifyError,
  tooMany,
  stage,
}: {
  busy: boolean;
  setActive: (active: boolean) => void;
  notifyError: (text: string) => void;
  /** What to say when more than one file is dropped. */
  tooMany: string;
  stage: (file: File) => void;
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
      setActive(false);

      const files = event.dataTransfer?.files;
      if (!files || files.length === 0) return;
      if (files.length !== 1) {
        notifyError(tooMany);
        return;
      }

      stage(files[0]);
    },
    onDragEnter(event) {
      if (!Array.from(event.dataTransfer?.types ?? []).includes('Files')) return;
      event.preventDefault();
      event.stopPropagation();
      dragDepthRef.current += 1;
      setActive(true);
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
      if (dragDepthRef.current === 0) setActive(false);
    },
  };
}
