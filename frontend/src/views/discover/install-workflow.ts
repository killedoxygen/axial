import type { ContentSelection, ResolutionPlan } from '../../types-content';
import type { AddOutcome } from './actions';

type InstallActions = Pick<typeof import('./actions'), 'addToInstance' | 'commitInstall'>;

export interface InstallWorkflowSnapshot {
  busy: boolean;
  plan: ResolutionPlan | null;
  /** Staged selections a plan without the unavailable ones would keep. */
  installable: ContentSelection[];
  staged: number;
}

/** Selections not blocked by an unavailable conflict, directly or through a
 * required dependency. Empty when none or all would be blocked or a blocked
 * project cannot be identified. */
export function installableSelections(selections: ContentSelection[], plan: ResolutionPlan): ContentSelection[] {
  const unavailable = plan.conflicts.filter((conflict) => conflict.kind === 'unavailable');
  if (unavailable.length === 0 || unavailable.some((conflict) => !conflict.canonical_id)) return [];
  const blocked = new Set(unavailable.map((conflict) => conflict.canonical_id!));
  const items = new Map(plan.items.map((item) => [item.canonical_id, item]));
  const isBlocked = (id: string, seen: Set<string>): boolean => {
    if (blocked.has(id)) return true;
    if (seen.has(id)) return false;
    seen.add(id);
    return (items.get(id)?.dependencies ?? []).some(
      (dependency) =>
        dependency.kind === 'required' &&
        dependency.project_id !== undefined &&
        isBlocked(`modrinth:${dependency.project_id}`, seen),
    );
  };
  const kept = selections.filter((selection) => !isBlocked(selection.canonical_id, new Set()));
  return kept.length === selections.length ? [] : kept;
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

  /** Plan again without the unavailable selections; the new plan decides. */
  const installRest = async (): Promise<AddOutcome> => {
    if (!pending || busy) return { status: 'failed' };
    const staged = pending;
    const rest = installableSelections(staged.selections, staged.plan);
    if (rest.length === 0) return { status: 'failed' };
    pending = null;
    return add(rest, staged.label);
  };

  return {
    snapshot: (): InstallWorkflowSnapshot => ({
      busy,
      plan: pending?.plan ?? null,
      installable: pending ? installableSelections(pending.selections, pending.plan) : [],
      staged: pending?.selections.length ?? 0,
    }),
    setTarget,
    add,
    confirm,
    installRest,
    cancel,
    dispose: reset,
  };
}
