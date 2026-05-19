import type { ReadonlySignal } from '@preact/signals';
import { createMachineSignal, matches, type SignalMachine } from '../machine';
import {
  clearLoaderCaches,
  fetchLoaderBuilds,
  fetchLoaderComponents,
  fetchLoaderSupportedVersions,
} from '../loaders/api';
import { pickPreferredBuild } from '../loaders/view-model';
import {
  initialNewInstanceLoaderState,
  transitionNewInstanceLoader,
  type LoaderMachineEvent,
  type NewInstanceLoaderState,
} from './new-instance-loader-core';
import type {
  LoaderBuildRecord,
  LoaderComponentId,
  LoaderComponentRecord,
  LoaderGameVersion,
} from '../types';

export type { NewInstanceLoaderState } from './new-instance-loader-core';

export interface NewInstanceLoaderMachine {
  state: ReadonlySignal<NewInstanceLoaderState>;
  enable(selectedMcVersion: string | null): Promise<void>;
  disable(): void;
  reset(): void;
  changeComponent(componentId: LoaderComponentId, selectedMcVersion: string | null): Promise<void>;
  changeMcVersion(mcVersion: string): Promise<void>;
  selectBuild(buildId: string): void;
  prefetchComponent(componentId: LoaderComponentId, mcVersions?: string[]): Promise<void>;
  prefetchBuilds(mcVersions: string[]): void;
}

function nextRequestId(machine: SignalMachine<NewInstanceLoaderState, LoaderMachineEvent>): number {
  return machine.state.value.context.requestId + 1;
}

function hasComponent(
  components: LoaderComponentRecord[],
  componentId: LoaderComponentId,
): boolean {
  return components.some((component) => component.id === componentId);
}

function isActiveRequest(
  machine: SignalMachine<NewInstanceLoaderState, LoaderMachineEvent>,
  requestId: number,
): boolean {
  const state = machine.state.value;
  return !matches(state, 'disabled') && state.context.requestId === requestId;
}

async function warmComponent(componentId: LoaderComponentId, mcVersions: string[]): Promise<void> {
  const components = await fetchLoaderComponents();
  if (!hasComponent(components, componentId)) {
    return;
  }
  const supportedVersions = await fetchLoaderSupportedVersions(componentId);
  await prefetchSupportedBuilds(componentId, supportedVersions, mcVersions, 2);
}

async function prefetchSupportedBuilds(
  componentId: LoaderComponentId,
  supportedVersions: LoaderGameVersion[],
  mcVersions: string[],
  limit: number,
): Promise<void> {
  if (mcVersions.length === 0 || limit <= 0) {
    return;
  }
  const supported = new Set(supportedVersions.map((entry) => entry.id));
  await Promise.allSettled(
    mcVersions
      .filter((version) => supported.has(version))
      .slice(0, limit)
      .map((version) => fetchLoaderBuilds(componentId, version)),
  );
}

function resolvePreferredComponentId(
  components: LoaderComponentRecord[],
  preferredComponentId: LoaderComponentId | null,
): LoaderComponentId | null {
  if (preferredComponentId && hasComponent(components, preferredComponentId)) {
    return preferredComponentId;
  }
  return components[0]?.id ?? null;
}

function resolveSelectedMcVersion(
  supportedVersions: LoaderGameVersion[],
  selectedMcVersion: string | null,
): string | null {
  if (!selectedMcVersion) {
    return null;
  }
  return supportedVersions.some((entry) => entry.id === selectedMcVersion)
    ? selectedMcVersion
    : null;
}

export function createNewInstanceLoaderMachine(): NewInstanceLoaderMachine {
  const machine = createMachineSignal(initialNewInstanceLoaderState(), transitionNewInstanceLoader);

  async function loadComponents(
    preferredComponentId: LoaderComponentId | null,
    selectedMcVersion: string | null,
  ): Promise<void> {
    const requestId = nextRequestId(machine);
    machine.dispatch({ type: 'start_components', requestId });
    try {
      const components = await fetchLoaderComponents();
      const selectedComponentId = resolvePreferredComponentId(components, preferredComponentId);
      machine.dispatch({
        type: 'components_loaded',
        requestId,
        components,
        selectedComponentId,
        selectedMcVersion,
      });
      if (!selectedComponentId || !isActiveRequest(machine, requestId)) {
        return;
      }
      await loadSupportedVersions(selectedComponentId, selectedMcVersion);
    } catch (error: unknown) {
      machine.dispatch({
        type: 'components_failed',
        requestId,
        errorMessage: error instanceof Error ? error.message : 'Failed to load loader components.',
      });
    }
  }

  async function loadSupportedVersions(
    selectedComponentId: LoaderComponentId,
    selectedMcVersion: string | null,
  ): Promise<void> {
    const requestId = nextRequestId(machine);
    machine.dispatch({
      type: 'start_versions',
      selectedComponentId,
      requestId,
    });
    try {
      const supportedVersions = await fetchLoaderSupportedVersions(selectedComponentId);
      const nextSelectedMcVersion = resolveSelectedMcVersion(supportedVersions, selectedMcVersion);
      machine.dispatch({
        type: 'versions_loaded',
        requestId,
        selectedComponentId,
        supportedVersions,
        selectedMcVersion: nextSelectedMcVersion,
      });
      if (!nextSelectedMcVersion || !isActiveRequest(machine, requestId)) {
        return;
      }
      await loadBuilds(selectedComponentId, nextSelectedMcVersion);
    } catch (error: unknown) {
      machine.dispatch({
        type: 'versions_failed',
        requestId,
        selectedComponentId,
        errorMessage: error instanceof Error ? error.message : 'Failed to load supported Minecraft versions.',
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
      await loadComponents(machine.state.value.context.selectedComponentId, selectedMcVersion);
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
        await loadComponents(componentId, selectedMcVersion);
        return;
      }
      await loadSupportedVersions(componentId, selectedMcVersion);
    },
    async changeMcVersion(mcVersion: string): Promise<void> {
      const selectedComponentId = machine.state.value.context.selectedComponentId;
      const supportedVersions = machine.state.value.context.supportedVersions;
      if (!selectedComponentId || !supportedVersions) {
        return;
      }
      if (!supportedVersions.some((entry) => entry.id === mcVersion)) {
        return;
      }
      await loadBuilds(selectedComponentId, mcVersion);
    },
    selectBuild(buildId: string): void {
      machine.dispatch({ type: 'select_build', buildId });
    },
    async prefetchComponent(componentId: LoaderComponentId, mcVersions: string[] = []): Promise<void> {
      await warmComponent(componentId, mcVersions);
    },
    prefetchBuilds(mcVersions: string[]): void {
      const selectedComponentId = machine.state.value.context.selectedComponentId;
      const supportedVersions = machine.state.value.context.supportedVersions;
      if (!selectedComponentId || !supportedVersions || mcVersions.length === 0) {
        return;
      }
      void prefetchSupportedBuilds(selectedComponentId, supportedVersions, mcVersions, 8);
    },
  };
}
