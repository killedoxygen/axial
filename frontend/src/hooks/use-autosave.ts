import { useEffect, useRef, useState } from 'preact/hooks';
import { setConfig } from '../actions';
import { api } from '../api';
import { configResponse } from '../dto-core';
import { config } from '../store';
import type { Config } from '../types-settings';
import { toast } from '../toast';
import { errMessage } from '../utils';

let configWrites: Promise<void> = Promise.resolve();
const targetWrites = new Map<string, Promise<void>>();

export function saveConfigPatch(patch: Record<string, unknown>, isCurrent?: () => boolean): Promise<Config> {
  const changesIdentity = Object.prototype.hasOwnProperty.call(patch, 'username')
    || Object.prototype.hasOwnProperty.call(patch, 'launch_auth_mode');
  const accountSelectionRevision = changesIdentity ? config.value?.account_selection_revision : undefined;
  const pending = configWrites.then(async () => {
    let current = config.value;
    if (!current) {
      const loaded = configResponse(await api('GET', '/config'));
      setConfig(loaded);
      current = config.value ?? loaded;
    }
    const accountFence = changesIdentity
      ? { expected_account_selection_revision: accountSelectionRevision ?? current.account_selection_revision }
      : {};
    if (isCurrent && !isCurrent()) throw new Error('Settings change was superseded.');
    try {
      const saved = configResponse(
        await api('PUT', '/config', { ...patch, ...accountFence, expected_revision: current.revision }),
      );
      setConfig(saved);
      return saved;
    } catch (error) {
      // An unsuccessful response does not prove the write had no effect.
      try {
        setConfig(configResponse(await api('GET', '/config')));
      } catch {
        // Preserve the last acknowledged snapshot until reads recover.
      }
      throw error;
    }
  });
  configWrites = pending.then(() => {}, () => {});
  return pending;
}

export function useAutoSave<TResp extends { error?: string }>({
  send,
  apply,
  errorLabel,
  target = '/config',
}: {
  send: (patch: Record<string, unknown>) => Promise<TResp>;
  apply: (resp: TResp) => void;
  errorLabel: string;
  target?: string;
}): {
  commit: (
    patch: Record<string, unknown>,
    opts?: { revert?: () => void; onSuccess?: () => void; label?: string },
  ) => void;
  saving: boolean;
} {
  const requestRef = useRef(0);
  const pendingRef = useRef(0);
  const latestFields = useRef(new Map<string, number>());
  const mounted = useRef(true);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const commit = (
    patch: Record<string, unknown>,
    opts?: { revert?: () => void; onSuccess?: () => void; label?: string },
  ): void => {
    const requestId = ++requestRef.current;
    const fields = Object.keys(patch);
    for (const field of fields) latestFields.current.set(field, requestId);
    pendingRef.current += 1;
    if (mounted.current) setSaving(true);
    const pending = (targetWrites.get(target) ?? Promise.resolve()).then(async () => {
      try {
        const res = await send(patch);
        if (res?.error) throw new Error(res.error);
        apply(res);
        toast('Saved');
        if (mounted.current) opts?.onSuccess?.();
      } catch (err) {
        if (mounted.current && fields.every((field) => latestFields.current.get(field) === requestId)) {
          opts?.revert?.();
        }
        toast(`Could not save ${opts?.label ?? errorLabel}: ${errMessage(err)}`, 'error');
      } finally {
        pendingRef.current -= 1;
        if (mounted.current) setSaving(pendingRef.current > 0);
      }
    });
    targetWrites.set(target, pending);
    void pending.finally(() => {
      if (targetWrites.get(target) === pending) targetWrites.delete(target);
    });
  };

  return { commit, saving };
}
