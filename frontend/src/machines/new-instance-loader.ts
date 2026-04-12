import type { ReadonlySignal } from '@preact/signals';
import { createMachineSignal, matches, type SignalMachine } from '../machine';
import { clearLoaderCaches, fetchLoaderBuilds, fetchLoaderComponents } from '../loaders/api';
import { pickPreferredBuild } from '../loaders/view-model';
import type { LoaderBuildRecord, LoaderComponentId, LoaderComponentRecord } from '../types';

type LoaderMachineContext = {
  components: LoaderComponentRecord[] | null;
  selectedComponentId: LoaderComponentId | null;
  selectedMcVersion: string | null;
  builds: LoaderBuildRecord[] | null;
  selectedBuildId: string | null;
  errorMessage: string | null;
  requestId: number;
};

export type NewInstanceLoaderState =
  | { kind: 'disabled'; context: LoaderMachineContext }
  | { kind: 'loading_components'; context: LoaderMachineContext }
  | { kind: 'selecting_version'; context: LoaderMachineContext }
  | { kind: 'loading_builds'; context: LoaderMachineContext }
  | { kind: 'ready'; context: LoaderMachineContext }
  | { kind: 'error'; stage: 'components' | 'builds'; context: LoaderMachineContext };

type LoaderMachineEvent =
  | { type: 'reset' }
  | { type: 'disable' }
  | { type: 'start_components'; requestId: number }
  | { type: 'components_loaded'; requestId: number; components: LoaderComponentRecord[]; selectedComponentId: LoaderComponentId | null; selectedMcVersion: string | null }
  | { type: 'components_failed'; requestId: number; errorMessage: string }
  | { type: 'start_builds'; selectedComponentId: LoaderComponentId; selectedMcVersion: string; requestId: number }
  | { type: 'builds_loaded'; requestId: number; selectedComponentId: LoaderComponentId; selectedMcVersion: string; builds: LoaderBuildRecord[]; selectedBuildId: string | null }
  | { type: 'builds_failed'; requestId: number; selectedComponentId: LoaderComponentId; selectedMcVersion: string; errorMessage: string }
  | { type: 'select_build'; buildId: string };

export interface NewInstanceLoaderMachine {
  state: ReadonlySignal<NewInstanceLoaderState>;
  enable(selectedMcVersion: string | null): Promise<void>;
  disable(): void;
  reset(): void;
  changeComponent(componentId: LoaderComponentId, selectedMcVersion: string | null): Promise<void>;
  changeMcVersion(mcVersion: string): Promise<void>;
  selectBuild(buildId: string): void;
}

const INITIAL_CONTEXT: LoaderMachineContext = {
  components: null,
  selectedComponentId: null,
  selectedMcVersion: null,
  builds: null,
  selectedBuildId: null,
  errorMessage: null,
  requestId: 0,
};

function initialState(): NewInstanceLoaderState {
  return {
    kind: 'disabled',
    context: { ...INITIAL_CONTEXT },
  };
}

function transition(
  state: NewInstanceLoaderState,
  event: LoaderMachineEvent,
): NewInstanceLoaderState {
  switch (event.type) {
    case 'reset':
      return initialState();
    case 'disable':
      return {
        kind: 'disabled',
        context: {
          ...state.context,
          selectedMcVersion: null,
          builds: null,
          selectedBuildId: null,
          errorMessage: null,
        },
      };
    case 'start_components':
      return {
        kind: 'loading_components',
        context: {
          ...state.context,
          builds: null,
          selectedBuildId: null,
          errorMessage: null,
          requestId: event.requestId,
        },
      };
    case 'components_loaded':
      if (!matches(state, 'loading_components') || state.context.requestId !== event.requestId) {
        return state;
      }
      return {
        kind: event.selectedMcVersion ? 'loading_builds' : 'selecting_version',
        context: {
          ...state.context,
          components: event.components,
          selectedComponentId: event.selectedComponentId,
          selectedMcVersion: event.selectedMcVersion,
          builds: null,
          selectedBuildId: null,
          errorMessage: null,
        },
      };
    case 'components_failed':
      if (!matches(state, 'loading_components') || state.context.requestId !== event.requestId) {
        return state;
      }
      return {
        kind: 'error',
        stage: 'components',
        context: {
          ...state.context,
          errorMessage: event.errorMessage,
        },
      };
    case 'start_builds':
      if (matches(state, 'disabled')) {
        return state;
      }
      return {
        kind: 'loading_builds',
        context: {
          ...state.context,
          selectedComponentId: event.selectedComponentId,
          selectedMcVersion: event.selectedMcVersion,
          builds: null,
          selectedBuildId: null,
          errorMessage: null,
          requestId: event.requestId,
        },
      };
    case 'builds_loaded':
      if (!matches(state, 'loading_builds') || state.context.requestId !== event.requestId) {
        return state;
      }
      if (!event.selectedBuildId) {
        return {
          kind: 'error',
          stage: 'builds',
          context: {
            ...state.context,
            selectedComponentId: event.selectedComponentId,
            selectedMcVersion: event.selectedMcVersion,
            builds: event.builds,
            selectedBuildId: null,
            errorMessage: 'No loader build is available for this Minecraft version.',
          },
        };
      }
      return {
        kind: 'ready',
        context: {
          ...state.context,
          selectedComponentId: event.selectedComponentId,
          selectedMcVersion: event.selectedMcVersion,
          builds: event.builds,
          selectedBuildId: event.selectedBuildId,
          errorMessage: null,
        },
      };
    case 'builds_failed':
      if (!matches(state, 'loading_builds') || state.context.requestId !== event.requestId) {
        return state;
      }
      return {
        kind: 'error',
        stage: 'builds',
        context: {
          ...state.context,
          selectedComponentId: event.selectedComponentId,
          selectedMcVersion: event.selectedMcVersion,
          builds: null,
          selectedBuildId: null,
          errorMessage: event.errorMessage,
        },
      };
    case 'select_build':
      if (!matches(state, 'ready')) {
        return state;
      }
      return {
        kind: 'ready',
        context: {
          ...state.context,
          selectedBuildId: event.buildId,
        },
      };
    default:
      return state;
  }
}

function nextRequestId(machine: SignalMachine<NewInstanceLoaderState, LoaderMachineEvent>): number {
  return machine.state.value.context.requestId + 1;
}

export function createNewInstanceLoaderMachine(): NewInstanceLoaderMachine {
  const machine = createMachineSignal(initialState(), transition);

  async function loadComponents(selectedMcVersion: string | null): Promise<void> {
    const requestId = nextRequestId(machine);
    machine.dispatch({ type: 'start_components', requestId });
    try {
      const components = await fetchLoaderComponents();
      const selectedComponentId = machine.state.value.context.selectedComponentId || components[0]?.id || null;
      machine.dispatch({
        type: 'components_loaded',
        requestId,
        components,
        selectedComponentId,
        selectedMcVersion,
      });
      if (selectedComponentId && selectedMcVersion) {
        await loadBuilds(selectedComponentId, selectedMcVersion);
      }
    } catch (error: unknown) {
      machine.dispatch({
        type: 'components_failed',
        requestId,
        errorMessage: error instanceof Error ? error.message : 'Failed to load loader components.',
      });
    }
  }

  async function loadBuilds(
    selectedComponentId: LoaderComponentId,
    selectedMcVersion: string,
  ): Promise<void> {
    const requestId = nextRequestId(machine);
    machine.dispatch({
      type: 'start_builds',
      selectedComponentId,
      selectedMcVersion,
      requestId,
    });
    try {
      const builds = await fetchLoaderBuilds(selectedComponentId, selectedMcVersion);
      const preferred = pickPreferredBuild(builds);
      machine.dispatch({
        type: 'builds_loaded',
        requestId,
        selectedComponentId,
        selectedMcVersion,
        builds,
        selectedBuildId: preferred?.build_id ?? null,
      });
    } catch (error: unknown) {
      machine.dispatch({
        type: 'builds_failed',
        requestId,
        selectedComponentId,
        selectedMcVersion,
        errorMessage: error instanceof Error ? error.message : 'Failed to load loader builds.',
      });
    }
  }

  return {
    state: machine.state,
    async enable(selectedMcVersion: string | null): Promise<void> {
      await loadComponents(selectedMcVersion);
    },
    disable(): void {
      machine.dispatch({ type: 'disable' });
    },
    reset(): void {
      clearLoaderCaches();
      machine.dispatch({ type: 'reset' });
    },
    async changeComponent(componentId: LoaderComponentId, selectedMcVersion: string | null): Promise<void> {
      if (!machine.state.value.context.components) {
        await loadComponents(selectedMcVersion);
        return;
      }
      if (!selectedMcVersion) {
        machine.dispatch({
          type: 'start_builds',
          selectedComponentId: componentId,
          selectedMcVersion: '',
          requestId: nextRequestId(machine),
        });
        machine.dispatch({
          type: 'builds_failed',
          selectedComponentId: componentId,
          selectedMcVersion: '',
          requestId: machine.state.value.context.requestId,
          errorMessage: 'Pick a Minecraft version first.',
        });
        return;
      }
      await loadBuilds(componentId, selectedMcVersion);
    },
    async changeMcVersion(mcVersion: string): Promise<void> {
      const selectedComponentId = machine.state.value.context.selectedComponentId;
      if (!selectedComponentId) {
        return;
      }
      await loadBuilds(selectedComponentId, mcVersion);
    },
    selectBuild(buildId: string): void {
      machine.dispatch({ type: 'select_build', buildId });
    },
  };
}
