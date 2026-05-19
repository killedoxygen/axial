import { matches } from '../machine';
import type {
  LoaderBuildRecord,
  LoaderComponentId,
  LoaderComponentRecord,
  LoaderGameVersion,
} from '../types';

export type LoaderMachineContext = {
  components: LoaderComponentRecord[] | null;
  supportedVersions: LoaderGameVersion[] | null;
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
  | { kind: 'loading_versions'; context: LoaderMachineContext }
  | { kind: 'selecting_version'; context: LoaderMachineContext }
  | { kind: 'loading_builds'; context: LoaderMachineContext }
  | { kind: 'ready'; context: LoaderMachineContext }
  | { kind: 'error'; stage: 'components' | 'versions' | 'builds'; context: LoaderMachineContext };

export type LoaderMachineEvent =
  | { type: 'reset' }
  | { type: 'disable' }
  | { type: 'start_components'; requestId: number }
  | {
    type: 'components_loaded';
    requestId: number;
    components: LoaderComponentRecord[];
    selectedComponentId: LoaderComponentId | null;
    selectedMcVersion: string | null;
  }
  | { type: 'components_failed'; requestId: number; errorMessage: string }
  | { type: 'start_versions'; selectedComponentId: LoaderComponentId; requestId: number }
  | {
    type: 'versions_loaded';
    requestId: number;
    selectedComponentId: LoaderComponentId;
    supportedVersions: LoaderGameVersion[];
    selectedMcVersion: string | null;
  }
  | {
    type: 'versions_failed';
    requestId: number;
    selectedComponentId: LoaderComponentId;
    errorMessage: string;
  }
  | {
    type: 'start_builds';
    selectedComponentId: LoaderComponentId;
    selectedMcVersion: string;
    requestId: number;
  }
  | {
    type: 'builds_loaded';
    requestId: number;
    selectedComponentId: LoaderComponentId;
    selectedMcVersion: string;
    builds: LoaderBuildRecord[];
    selectedBuildId: string | null;
  }
  | {
    type: 'builds_failed';
    requestId: number;
    selectedComponentId: LoaderComponentId;
    selectedMcVersion: string;
    errorMessage: string;
  }
  | { type: 'select_build'; buildId: string };

const INITIAL_CONTEXT: LoaderMachineContext = {
  components: null,
  supportedVersions: null,
  selectedComponentId: null,
  selectedMcVersion: null,
  builds: null,
  selectedBuildId: null,
  errorMessage: null,
  requestId: 0,
};

const RESET_SELECTION: Partial<LoaderMachineContext> = {
  supportedVersions: null,
  selectedMcVersion: null,
  builds: null,
  selectedBuildId: null,
  errorMessage: null,
};

const RESET_BUILDS: Partial<LoaderMachineContext> = {
  builds: null,
  selectedBuildId: null,
  errorMessage: null,
};

export function initialNewInstanceLoaderState(): NewInstanceLoaderState {
  return {
    kind: 'disabled',
    context: { ...INITIAL_CONTEXT },
  };
}

export function transitionNewInstanceLoader(
  state: NewInstanceLoaderState,
  event: LoaderMachineEvent,
): NewInstanceLoaderState {
  switch (event.type) {
    case 'reset':
      return initialNewInstanceLoaderState();
    case 'disable':
      return {
        kind: 'disabled',
        context: updateContext(state.context, RESET_SELECTION),
      };
    case 'start_components':
      return {
        kind: 'loading_components',
        context: updateContext(state.context, RESET_SELECTION, { requestId: event.requestId }),
      };
    case 'components_loaded':
      if (!matchesRequest(state, 'loading_components', event.requestId)) {
        return state;
      }
      return {
        kind: event.selectedComponentId ? 'loading_versions' : 'selecting_version',
        context: updateContext(state.context, RESET_SELECTION, {
          components: event.components,
          selectedComponentId: event.selectedComponentId,
          selectedMcVersion: event.selectedMcVersion,
        }),
      };
    case 'components_failed':
      if (!matchesRequest(state, 'loading_components', event.requestId)) {
        return state;
      }
      return errorState(state, 'components', {
        errorMessage: event.errorMessage,
      });
    case 'start_versions':
      if (matches(state, 'disabled')) {
        return state;
      }
      return {
        kind: 'loading_versions',
        context: updateContext(state.context, RESET_SELECTION, {
          selectedComponentId: event.selectedComponentId,
          requestId: event.requestId,
        }),
      };
    case 'versions_loaded':
      if (!matchesRequest(state, 'loading_versions', event.requestId)) {
        return state;
      }
      return {
        kind: event.selectedMcVersion ? 'loading_builds' : 'selecting_version',
        context: updateContext(state.context, RESET_BUILDS, {
          selectedComponentId: event.selectedComponentId,
          supportedVersions: event.supportedVersions,
          selectedMcVersion: event.selectedMcVersion,
        }),
      };
    case 'versions_failed':
      if (!matchesRequest(state, 'loading_versions', event.requestId)) {
        return state;
      }
      return errorState(state, 'versions', {
        selectedComponentId: event.selectedComponentId,
        supportedVersions: null,
        builds: null,
        selectedBuildId: null,
        errorMessage: event.errorMessage,
      });
    case 'start_builds':
      if (matches(state, 'disabled')) {
        return state;
      }
      return {
        kind: 'loading_builds',
        context: updateContext(state.context, RESET_BUILDS, {
          selectedComponentId: event.selectedComponentId,
          selectedMcVersion: event.selectedMcVersion,
          supportedVersions: state.context.supportedVersions,
          requestId: event.requestId,
        }),
      };
    case 'builds_loaded':
      if (!matchesRequest(state, 'loading_builds', event.requestId)) {
        return state;
      }
      if (!event.selectedBuildId) {
        return errorState(state, 'builds', {
          selectedComponentId: event.selectedComponentId,
          selectedMcVersion: event.selectedMcVersion,
          supportedVersions: state.context.supportedVersions,
          builds: event.builds,
          selectedBuildId: null,
          errorMessage: 'No loader build is available for this Minecraft version.',
        });
      }
      return {
        kind: 'ready',
        context: updateContext(state.context, {
          selectedComponentId: event.selectedComponentId,
          selectedMcVersion: event.selectedMcVersion,
          supportedVersions: state.context.supportedVersions,
          builds: event.builds,
          selectedBuildId: event.selectedBuildId,
          errorMessage: null,
        }),
      };
    case 'builds_failed':
      if (!matchesRequest(state, 'loading_builds', event.requestId)) {
        return state;
      }
      return errorState(state, 'builds', {
        selectedComponentId: event.selectedComponentId,
        selectedMcVersion: event.selectedMcVersion,
        supportedVersions: state.context.supportedVersions,
        builds: null,
        selectedBuildId: null,
        errorMessage: event.errorMessage,
      });
    case 'select_build':
      if (!matches(state, 'ready')) {
        return state;
      }
      return {
        kind: 'ready',
        context: updateContext(state.context, {
          selectedBuildId: event.buildId,
        }),
      };
    default:
      return state;
  }
}

function matchesRequest(
  state: NewInstanceLoaderState,
  kind: Extract<NewInstanceLoaderState, { kind: string }>['kind'],
  requestId: number,
): boolean {
  return matches(state, kind) && state.context.requestId === requestId;
}

function updateContext(
  context: LoaderMachineContext,
  ...updates: Array<Partial<LoaderMachineContext>>
): LoaderMachineContext {
  return Object.assign({}, context, ...updates);
}

function errorState(
  state: NewInstanceLoaderState,
  stage: 'components' | 'versions' | 'builds',
  update: Partial<LoaderMachineContext>,
): NewInstanceLoaderState {
  return {
    kind: 'error',
    stage,
    context: updateContext(state.context, update),
  };
}
