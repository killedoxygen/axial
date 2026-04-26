import { signal, type ReadonlySignal } from '@preact/signals';

export interface SignalMachine<State, Event> {
  state: ReadonlySignal<State>;
  dispatch(event: Event): void;
}

export function createMachineSignal<State, Event>(
  initialState: State,
  transition: (state: State, event: Event) => State,
): SignalMachine<State, Event> {
  const state = signal(initialState);
  return {
    state,
    dispatch(event: Event): void {
      state.value = transition(state.value, event);
    },
  };
}

export function matches<State extends { kind: string }, Kind extends State['kind']>(
  state: State,
  kind: Kind,
): state is Extract<State, { kind: Kind }> {
  return state.kind === kind;
}
