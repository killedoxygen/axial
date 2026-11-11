import type { JSX, RefObject } from 'preact';
import { useRef, useState } from 'preact/hooks';
import { contentFilesOfKind, useContentFileDrop } from '../../../hooks/use-content-file-drop';
import { pickNativeContentFiles } from '../../../native-content';
import { toast } from '../../../toast';
import { errMessage } from '../../../utils';
import { importModFiles } from '../mod-import';

/** Picker, drop surface and hidden browser input for adding local jars. */
export function useModImport(
  instanceId: string,
  surfaceRef: RefObject<HTMLElement | null>,
  onImported: () => void,
): {
  busy: boolean;
  dropActive: boolean;
  dropHandlers: ReturnType<typeof useContentFileDrop>['handlers'];
  pick: () => void;
  input: JSX.Element;
} {
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const inputRef = useRef<HTMLInputElement | null>(null);

  const add = (files: File[]): void => {
    if (busyRef.current) {
      toast('Wait for the current mod files to finish adding.', 'error');
      return;
    }
    busyRef.current = true;
    setBusy(true);
    void importModFiles(instanceId, files)
      .then((added) => {
        if (added) onImported();
      })
      .finally(() => {
        busyRef.current = false;
        setBusy(false);
      });
  };
  const fail = (message: string): void => toast(message, 'error');
  const drop = useContentFileDrop({ kind: 'mod', surfaceRef, enabled: !busy, onFiles: add, onError: fail });

  const pick = (): void => {
    if (busyRef.current) return;
    void pickNativeContentFiles('mod')
      .then((files) => {
        if (files === undefined) inputRef.current?.click();
        else if (files && files.length > 0) add(files);
      })
      .catch((error: unknown) => fail(errMessage(error)));
  };

  const input = (
    <input
      ref={inputRef}
      type="file"
      accept=".jar"
      multiple
      hidden
      onChange={(event) => {
        const chosen = Array.from(event.currentTarget.files ?? []);
        event.currentTarget.value = '';
        if (chosen.length === 0) return;
        const files = contentFilesOfKind(chosen, 'mod');
        if (typeof files === 'string') fail(files);
        else add(files);
      }}
    />
  );

  return { busy, dropActive: drop.active, dropHandlers: drop.handlers, pick, input };
}
