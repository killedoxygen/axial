import type { RefObject } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import {
  nativeDragPositionHitsElement,
  onNativeContentDragDrop,
  readNativeContentSelection,
  type NativeContentKind,
} from '../native-content';
import { errMessage } from '../utils';

const EXTENSION: Record<NativeContentKind, string> = { mod: '.jar', modpack: '.mrpack' };
const WRONG_FILES: Record<NativeContentKind, string> = {
  mod: 'Drop .jar mod files here.',
  modpack: 'Drop one .mrpack modpack file here.',
};

export function contentFilesOfKind(files: File[], kind: NativeContentKind): File[] | string {
  const matching = files.filter((file) => file.name.toLowerCase().endsWith(EXTENSION[kind]));
  if (matching.length === 0 || matching.length !== files.length) return WRONG_FILES[kind];
  if (kind === 'modpack' && matching.length !== 1) return WRONG_FILES[kind];
  return matching;
}

/** Browser drops arrive as DOM events; desktop drops arrive from the native shell
 * with a one-time selection the surface under the pointer reads. */
export function useContentFileDrop({
  kind,
  surfaceRef,
  enabled,
  onFiles,
  onError,
}: {
  kind: NativeContentKind;
  surfaceRef: RefObject<HTMLElement | null>;
  enabled: boolean;
  onFiles: (files: File[]) => void;
  onError: (message: string) => void;
}): {
  active: boolean;
  handlers: {
    onDragEnter: (event: DragEvent) => void;
    onDragOver: (event: DragEvent) => void;
    onDragLeave: (event: DragEvent) => void;
    onDrop: (event: DragEvent) => void;
  };
} {
  const [active, setActive] = useState(false);
  const depth = useRef(0);
  const latest = useRef({ enabled, onFiles, onError });
  latest.current = { enabled, onFiles, onError };

  useEffect(() => {
    let alive = true;
    let subscription: { close(): void } | null = null;
    void onNativeContentDragDrop((payload) => {
      if (!alive) return;
      const over =
        payload.kind === kind &&
        latest.current.enabled &&
        nativeDragPositionHitsElement(payload.position, surfaceRef.current);
      if (payload.type === 'enter' || payload.type === 'over') {
        setActive(over);
        return;
      }
      setActive(false);
      if (payload.type !== 'drop' || !over) return;
      if (payload.error) {
        latest.current.onError(payload.error);
        return;
      }
      if (!payload.selection) return;
      void readNativeContentSelection(payload.selection)
        .then((files) => {
          if (alive && files.length > 0) latest.current.onFiles(files);
        })
        .catch((error: unknown) => {
          if (alive) latest.current.onError(errMessage(error));
        });
    }).then((next) => {
      if (alive) subscription = next;
      else next?.close();
    });
    return () => {
      alive = false;
      subscription?.close();
    };
  }, [kind, surfaceRef]);

  const carriesFiles = (event: DragEvent): boolean => Array.from(event.dataTransfer?.types ?? []).includes('Files');
  return {
    active,
    handlers: {
      onDragEnter(event) {
        if (!carriesFiles(event) || !latest.current.enabled) return;
        event.preventDefault();
        depth.current += 1;
        setActive(true);
      },
      onDragOver(event) {
        if (!carriesFiles(event) || !latest.current.enabled) return;
        event.preventDefault();
        if (event.dataTransfer) event.dataTransfer.dropEffect = 'copy';
      },
      onDragLeave(event) {
        if (!carriesFiles(event)) return;
        depth.current = Math.max(0, depth.current - 1);
        if (depth.current === 0) setActive(false);
      },
      onDrop(event) {
        if (!carriesFiles(event) || !latest.current.enabled) return;
        event.preventDefault();
        depth.current = 0;
        setActive(false);
        const files = contentFilesOfKind(Array.from(event.dataTransfer?.files ?? []), kind);
        if (typeof files === 'string') latest.current.onError(files);
        else latest.current.onFiles(files);
      },
    },
  };
}
