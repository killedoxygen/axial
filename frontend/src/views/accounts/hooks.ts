import { useCallback, useEffect, useMemo, useState } from 'preact/hooks';
import { api, isApiError } from '../../api';
import { createAccountsSkinsMachine } from '../../machines/accounts-skins';
import { authStatusResponse, isRecord, launcherAccountsResponse, savedSkinsResponse } from './api';
import type { AuthStatusRecord, AuthStatusState, LauncherAccount, SavedSkinRecord } from './types';

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
      .then(async (res: unknown) => {
        if (!active) return;
        if (isRecord(res) && typeof res.error === 'string') throw new Error(res.error);
        let parsed = authStatusResponse(res);
        if (!parsed) throw new Error('invalid auth status');
        if (
          parsed.launch_auth_mode === 'online' &&
          !parsed.online_mode_ready &&
          parsed.msa_refresh_available
        ) {
          try {
            await api('POST', '/auth/refresh');
            if (!active) return;
            const refreshed = authStatusResponse(await api('GET', '/auth/status'));
            if (refreshed) parsed = refreshed;
          } catch (err: unknown) {
            // Keep the restored account visible; account actions can surface the
            // refresh failure when the user re-verifies or launches online.
            console.warn('Could not refresh restored Microsoft sign-in while loading accounts.', err);
          }
        }
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

export function useLauncherAccounts(): {
  accounts: LauncherAccount[];
  activeAccountId: string | null;
  state: AuthStatusState;
  refresh: () => void;
} {
  const [accounts, setAccounts] = useState<LauncherAccount[]>([]);
  const [activeAccountId, setActiveAccountId] = useState<string | null>(null);
  const [state, setState] = useState<AuthStatusState>('loading');
  const [refreshIndex, setRefreshIndex] = useState(0);

  const refresh = useCallback(() => {
    setRefreshIndex((value) => value + 1);
  }, []);

  useEffect(() => {
    let active = true;
    setState('loading');

    void api('GET', '/accounts')
      .then(async (res: unknown) => {
        if (!active) return;
        let parsed = launcherAccountsResponse(res);
        if (!parsed) throw new Error('invalid account list');
        const activeAccount = parsed.accounts.find((account) => account.active);
        const activeNeedsRefresh = activeAccount?.kind === 'microsoft' &&
          activeAccount.msa_refresh_available === true &&
          !(
            activeAccount.minecraft_profile_ready === true &&
            activeAccount.minecraft_ownership_verified === true &&
            typeof activeAccount.minecraft_token_expires_in === 'number' &&
            activeAccount.minecraft_token_expires_in > 0
          );
        if (activeNeedsRefresh) {
          try {
            await api('POST', '/auth/refresh');
            if (!active) return;
            const refreshed = launcherAccountsResponse(await api('GET', '/accounts'));
            if (refreshed) parsed = refreshed;
          } catch (err: unknown) {
            // Keep the restored account visible; account actions surface the
            // refresh failure when the user launches, refreshes, or re-verifies.
            console.warn('Could not refresh restored Microsoft account while loading account list.', err);
          }
        }
        setAccounts(parsed.accounts);
        setActiveAccountId(parsed.active_account_id);
        setState('ready');
      })
      .catch(() => {
        if (!active) return;
        setAccounts([]);
        setActiveAccountId(null);
        setState('unavailable');
      });

    return () => {
      active = false;
    };
  }, [refreshIndex]);

  return { accounts, activeAccountId, state, refresh };
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
