import type { JSX, RefObject } from 'preact';
import { useRef, useState } from 'preact/hooks';
import { identifyModpackFile } from './api';
import { contentFilesOfKind, useContentFileDrop } from '../content/use-file-drop';
import { pickNativeContentFiles } from '../content/native-files';
import { beginCreateIntent, openCreateModpack } from '../app/route';
import { errMessage } from '../app/toast';

export function useModpackFileImport(
  surfaceRef: RefObject<HTMLElement | null>,
  enabled: boolean,
): {
  busy: boolean;
  error: string | null;
  dropActive: boolean;
  dropHandlers: ReturnType<typeof useContentFileDrop>['handlers'];
  pick: () => void;
  input: JSX.Element;
} {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const busyRef = useRef(false);
  const inputRef = useRef<HTMLInputElement | null>(null);

  const open = (files: File[]): void => {
    const [file] = files;
    if (!file || busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    setError(null);
    const isCurrent = beginCreateIntent();
    void identifyModpackFile(file)
      .then((target) => {
        if (isCurrent()) openCreateModpack(target);
      })
      .catch((reason: unknown) => {
        if (isCurrent()) setError(errMessage(reason));
      })
      .finally(() => {
        busyRef.current = false;
        setBusy(false);
      });
  };
  const drop = useContentFileDrop({
    kind: 'modpack',
    surfaceRef,
    enabled: enabled && !busy,
    onFiles: open,
    onError: setError,
  });

  const pick = (): void => {
    if (busyRef.current) return;
    void pickNativeContentFiles('modpack')
      .then((files) => {
        if (files === undefined) inputRef.current?.click();
        else if (files) open(files);
      })
      .catch((reason: unknown) => setError(errMessage(reason)));
  };

  const input = (
    <input
      ref={inputRef}
      type="file"
      accept=".mrpack"
      hidden
      onChange={(event) => {
        const chosen = Array.from(event.currentTarget.files ?? []);
        event.currentTarget.value = '';
        if (chosen.length === 0) return;
        const files = contentFilesOfKind(chosen, 'modpack');
        if (typeof files === 'string') setError(files);
        else open(files);
      }}
    />
  );

  return { busy, error, dropActive: drop.active, dropHandlers: drop.handlers, pick, input };
}
