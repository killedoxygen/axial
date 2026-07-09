import { useRef, useState } from 'preact/hooks';
import { toast } from '../toast';
import { errMessage } from '../utils';

export function useAutoSave<TResp extends { error?: string }>({
  send,
  apply,
  errorLabel,
}: {
  send: (patch: Record<string, unknown>) => Promise<TResp>;
  apply: (resp: TResp) => void;
  errorLabel: string;
}): {
  commit: (
    patch: Record<string, unknown>,
    opts?: { revert?: () => void; onSuccess?: () => void; label?: string },
  ) => void;
  saving: boolean;
} {
  const requestRef = useRef(0);
  const [saving, setSaving] = useState(false);

  const commit = (
    patch: Record<string, unknown>,
    opts?: { revert?: () => void; onSuccess?: () => void; label?: string },
  ): void => {
    const requestId = ++requestRef.current;
    setSaving(true);
    void (async () => {
      try {
        const res = await send(patch);
        if (res?.error) throw new Error(res.error);
        if (requestId !== requestRef.current) return;
        apply(res);
        toast('Saved');
        opts?.onSuccess?.();
      } catch (err) {
        if (requestId !== requestRef.current) return;
        opts?.revert?.();
        toast(`Could not save ${opts?.label ?? errorLabel}: ${errMessage(err)}`, 'error');
      } finally {
        if (requestId === requestRef.current) setSaving(false);
      }
    })();
  };

  return { commit, saving };
}
