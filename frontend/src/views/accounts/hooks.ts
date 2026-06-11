import { useCallback, useEffect, useMemo, useState } from 'preact/hooks';
import { api, isApiError } from '../../api';
import { createAccountsSkinsMachine } from '../../machines/accounts-skins';
import { authStatusResponse, isRecord, savedSkinsResponse } from './api';
import type { AuthStatusRecord, AuthStatusState, SavedSkinRecord } from './types';

export function useAuthStatus(savedUsername: string): {
  status: AuthStatusRecord | null;
  state: AuthStatusState;
  refresh: () => void;
} {
  const [status, setStatus] = useState<AuthStatusRecord | null>(null);
  const [state, setState] = useState<AuthStatusState>('loading');
  const [refreshIndex, setRefreshIndex] = useState(0);

  const refresh = useCallback(() => {
    setRefreshIndex((value) => value + 1);
  }, []);

  useEffect(() => {
    let active = true;
    setState('loading');
    setStatus(null);

    void api('GET', '/auth/status')
      .then((res: unknown) => {
        if (!active) return;
        if (isRecord(res) && typeof res.error === 'string') throw new Error(res.error);
        const parsed = authStatusResponse(res);
        if (!parsed) throw new Error('invalid auth status');
        setStatus(parsed);
        setState('ready');
      })
      .catch(() => {
        if (!active) return;
        setStatus(null);
        setState('unavailable');
      });

    return () => {
      active = false;
    };
  }, [savedUsername, refreshIndex]);

  return { status, state, refresh };
}

export function useSavedSkins(): {
  skins: SavedSkinRecord[];
  pendingApplyKey: string | null;
  state: AuthStatusState;
  error: string | null;
  refresh: () => void;
  setPendingApplyKey: (textureKey: string | null) => void;
} {
  const machine = useMemo(() => createAccountsSkinsMachine(), []);
  const snapshot = machine.state.value;

  const refresh = useCallback(() => {
    const requestId = machine.nextRequestId();
    machine.dispatch({ type: 'load_started', requestId });

    void api('GET', '/skins')
      .then((res: unknown) => {
        const parsed = savedSkinsResponse(res);
        if (!parsed) throw new Error('invalid saved skins response');
        machine.dispatch({ type: 'load_succeeded', requestId, data: parsed });
      })
      .catch((err: unknown) => {
        machine.dispatch({
          type: 'load_failed',
          requestId,
          error: isApiError(err) && isRecord(err.payload) && typeof err.payload.error === 'string'
            ? err.payload.error
            : err instanceof Error ? err.message : 'Saved skins are unavailable.',
        });
      });
  }, [machine]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return {
    skins: snapshot.context.skins,
    pendingApplyKey: snapshot.context.pendingApplyKey,
    state: snapshot.kind === 'loading' ? 'loading' : snapshot.kind === 'error' ? 'unavailable' : 'ready',
    error: snapshot.context.error,
    refresh,
    setPendingApplyKey: (textureKey: string | null) => {
      machine.dispatch({ type: 'set_pending_apply', textureKey });
    },
  };
}
