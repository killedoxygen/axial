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
interface Write {
  started: boolean;
  done: Promise<boolean>;
}
const writes = new Map<Promise<unknown>, Write>();
const drafts = new Set<() => void>();
const preparations = new Set<{ phase: 'flush' | 'discard'; released: Promise<void> }>();
let flushingDrafts = false;
const pausedMessage = 'Settings changes are paused while the desktop operation finishes.';

export function canAutoSave(): boolean {
  return preparations.size === 0 || flushingDrafts;
}

export function registerAutoSaveDraft(flush: () => void): () => void {
  drafts.add(flush);
  return () => {
    drafts.delete(flush);
  };
}

function trackWrite(pending: Promise<unknown>, work: Write): void {
  work.done = pending.then(
    () => true,
    () => false,
  );
  writes.set(pending, work);
  void pending
    .then(
      () => {},
      () => {},
    )
    .then(() => work.done)
    .then(() => writes.delete(pending));
}

function startWrite<T>(work: Write, send: () => Promise<T>): Promise<T> {
  const blocked = [...preparations].filter((preparation) => preparation.phase === 'discard');
  if (blocked.length)
    return Promise.all(blocked.map((preparation) => preparation.released)).then(() => startWrite(work, send));
  work.started = true;
  return send();
}

export function prepareAutoSaves(phase: 'flush' | 'discard'): { done: Promise<boolean>; release: () => void } {
  let release!: () => void;
  const preparation = {
    phase,
    released: new Promise<void>((resolve) => {
      release = resolve;
    }),
  };
  preparations.add(preparation);
  if (phase === 'flush') {
    flushingDrafts = true;
    try {
      for (const flush of drafts) flush();
    } finally {
      flushingDrafts = false;
    }
  }
  const pending = [...writes.values()].filter((work) => phase === 'flush' || work.started).map((work) => work.done);
  return {
    done: Promise.all(pending).then((results) => phase === 'discard' || results.every(Boolean)),
    release() {
      preparations.delete(preparation);
      release();
      if (canAutoSave()) for (const flush of drafts) flush();
    },
  };
}

export function saveConfigPatch(patch: Record<string, unknown>, isCurrent?: () => boolean): Promise<Config> {
  if (!canAutoSave()) return Promise.reject(new Error(pausedMessage));
  const work: Write = { started: false, done: Promise.resolve(true) };
  const changesIdentity =
    Object.prototype.hasOwnProperty.call(patch, 'username') ||
    Object.prototype.hasOwnProperty.call(patch, 'launch_auth_mode');
  const accountSelectionRevision = changesIdentity ? config.value?.account_selection_revision : undefined;
  const pending = configWrites.then(async () => {
    let current = config.value;
    if (!current) {
      const loaded = configResponse(await api('GET', '/config'));
      setConfig(loaded);
      current = config.value ?? loaded;
    }
    const loadedConfig = current;
    return startWrite(work, async () => {
      const latest = config.value ?? loadedConfig;
      const accountFence = changesIdentity
        ? { expected_account_selection_revision: accountSelectionRevision ?? latest.account_selection_revision }
        : {};
      if (isCurrent && !isCurrent()) throw new Error('Settings change was superseded.');
      try {
        const saved = configResponse(
          await api('PUT', '/config', { ...patch, ...accountFence, expected_revision: latest.revision }),
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
  });
  trackWrite(pending, work);
  configWrites = pending.then(
    () => {},
    () => {},
  );
  return pending;
}

export function useAutoSave<TResp extends { error?: string }>({
  send,
  apply,
  errorLabel,
  target = '/config',
  flushPending,
}: {
  send: (patch: Record<string, unknown>) => Promise<TResp>;
  apply: (resp: TResp) => void;
  errorLabel: string;
  target?: string;
  flushPending?: () => void;
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
  const pendingDraft = useRef(flushPending);
  pendingDraft.current = flushPending;
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    mounted.current = true;
    const flush = (): void => {
      pendingDraft.current?.();
      if (!mounted.current) drafts.delete(flush);
    };
    const removeDraft = registerAutoSaveDraft(flush);
    return () => {
      mounted.current = false;
      pendingDraft.current?.();
      if (!pendingDraft.current || canAutoSave()) removeDraft();
    };
  }, []);

  const commit = (
    patch: Record<string, unknown>,
    opts?: { revert?: () => void; onSuccess?: () => void; label?: string },
  ): void => {
    if (!canAutoSave()) {
      opts?.revert?.();
      toast(pausedMessage, 'error');
      return;
    }
    const requestId = ++requestRef.current;
    const fields = Object.keys(patch);
    for (const field of fields) latestFields.current.set(field, requestId);
    pendingRef.current += 1;
    if (mounted.current) setSaving(true);
    // Config already has a revision-fenced queue; do not park it behind a second queue.
    const configWrite = Object.is(send, saveConfigPatch);
    const sent = configWrite
      ? send(patch)
      : (() => {
          const work: Write = { started: false, done: Promise.resolve(true) };
          const pending = (targetWrites.get(target) ?? Promise.resolve()).then(() =>
            startWrite(work, () => send(patch)),
          );
          trackWrite(pending, work);
          return pending;
        })();
    const pending = (async () => {
      try {
        const res = await sent;
        if (res?.error) throw new Error(res.error);
        apply(res);
        toast('Saved');
        if (mounted.current) opts?.onSuccess?.();
        return true;
      } catch (err) {
        if (mounted.current && fields.every((field) => latestFields.current.get(field) === requestId)) {
          opts?.revert?.();
        }
        toast(`Could not save ${opts?.label ?? errorLabel}: ${errMessage(err)}`, 'error');
        return false;
      } finally {
        pendingRef.current -= 1;
        if (mounted.current) setSaving(pendingRef.current > 0);
      }
    })();
    writes.get(sent)!.done = pending;
    const tail = pending.then(() => {});
    if (!configWrite) targetWrites.set(target, tail);
    void tail.then(() => {
      if (targetWrites.get(target) === tail) targetWrites.delete(target);
    });
  };

  return { commit, saving };
}
