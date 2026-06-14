import { createMachineSignal, type SignalMachine } from '../machine';

export interface AccountsSkinRecord {
  texture_key: string;
  name: string;
  variant: 'classic' | 'slim';
  source: string;
  cape_id: string | null;
  created_at: string;
  updated_at: string;
  applied_at: string | null;
  byte_size: number;
}

export interface AccountsSkinsData {
  skins: AccountsSkinRecord[];
  pendingApplyKey: string | null;
}

export interface AccountsSkinsContext {
  skins: AccountsSkinRecord[];
  pendingApplyKey: string | null;
  error: string | null;
  requestId: number;
}

export type AccountsSkinsState =
  | { kind: 'loading'; context: AccountsSkinsContext }
  | { kind: 'ready'; context: AccountsSkinsContext }
  | { kind: 'error'; context: AccountsSkinsContext };

export type AccountsSkinsEvent =
  | { type: 'load_started'; requestId: number }
  | { type: 'load_succeeded'; requestId: number; data: AccountsSkinsData }
  | { type: 'load_failed'; requestId: number; error: string }
  | { type: 'set_pending_apply'; textureKey: string | null };

export interface AccountsSkinsMachine extends SignalMachine<AccountsSkinsState, AccountsSkinsEvent> {
  nextRequestId(): number;
}

const INITIAL_CONTEXT: AccountsSkinsContext = {
  skins: [],
  pendingApplyKey: null,
  error: null,
  requestId: 0,
};

export function initialAccountsSkinsState(): AccountsSkinsState {
  return { kind: 'loading', context: { ...INITIAL_CONTEXT } };
}

export function transitionAccountsSkins(state: AccountsSkinsState, event: AccountsSkinsEvent): AccountsSkinsState {
  switch (event.type) {
    case 'load_started':
      return {
        kind: 'loading',
        context: { ...state.context, error: null, requestId: event.requestId },
      };
    case 'load_succeeded':
      if (event.requestId !== state.context.requestId) return state;
      return {
        kind: 'ready',
        context: {
          skins: event.data.skins,
          pendingApplyKey: event.data.pendingApplyKey,
          error: null,
          requestId: event.requestId,
        },
      };
    case 'load_failed':
      if (event.requestId !== state.context.requestId) return state;
      return {
        kind: 'error',
        context: { skins: [], pendingApplyKey: null, error: event.error, requestId: event.requestId },
      };
    case 'set_pending_apply':
      return {
        ...state,
        context: { ...state.context, pendingApplyKey: event.textureKey },
      };
    default:
      return state;
  }
}

export function createAccountsSkinsMachine(): AccountsSkinsMachine {
  const machine = createMachineSignal(initialAccountsSkinsState(), transitionAccountsSkins);
  return {
    ...machine,
    nextRequestId(): number {
      return machine.state.value.context.requestId + 1;
    },
  };
}
