import type { ContentSelection, ResolutionPlan } from '../../types-content';
import type { AddOutcome } from './actions';

type InstallActions = Pick<typeof import('./actions'), 'addToInstance' | 'commitInstall'>;

export interface InstallWorkflowSnapshot {
  busy: boolean;
  plan: ResolutionPlan | null;
}

/** Owns the pending confirmation, never the accepted backend operation. */
export function createInstallWorkflow(actions: InstallActions, onChange: () => void) {
  let instanceId: string | undefined;
  let generation = 0;
  let busy = false;
  let pending: { selections: ContentSelection[]; label: string; plan: ResolutionPlan } | null = null;

  const reset = (): void => {
    generation += 1;
    busy = false;
    pending = null;
  };

  const setTarget = (next: string | undefined): void => {
    if (next === instanceId) return;
    instanceId = next;
    reset();
  };

  const add = async (selections: ContentSelection[], label: string): Promise<AddOutcome> => {
    if (!instanceId || busy || pending) return { status: 'failed' };
    const target = instanceId;
    const requestGeneration = generation;
    const captured = selections.map((selection) => ({ ...selection }));
    busy = true;
    onChange();
    try {
      const outcome = await actions.addToInstance(target, captured, label);
      if (requestGeneration !== generation) return { status: 'superseded' };
      if (outcome.status === 'needs-confirmation' && outcome.plan) {
        pending = { selections: captured, label, plan: outcome.plan };
      }
      return outcome;
    } finally {
      if (requestGeneration === generation) {
        busy = false;
        onChange();
      }
    }
  };

  const confirm = async (): Promise<AddOutcome> => {
    if (!instanceId || !pending || busy) return { status: 'failed' };
    const target = instanceId;
    const staged = pending;
    const requestGeneration = generation;
    busy = true;
    onChange();
    try {
      const outcome = await actions.commitInstall(target, staged.selections, staged.label, staged.plan, true);
      if (requestGeneration !== generation) return { status: 'superseded' };
      pending = null;
      return outcome;
    } finally {
      if (requestGeneration === generation) {
        busy = false;
        onChange();
      }
    }
  };

  const cancel = (): void => {
    if (busy) return;
    reset();
    onChange();
  };

  return {
    snapshot: (): InstallWorkflowSnapshot => ({ busy, plan: pending?.plan ?? null }),
    setTarget,
    add,
    confirm,
    cancel,
    dispose: reset,
  };
}
