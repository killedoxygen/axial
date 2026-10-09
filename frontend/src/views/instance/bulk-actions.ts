import { showChoice } from '../../ui/Dialog';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import { signal } from '@preact/signals';

export type ResourceMutationState =
  | { status: 'idle' }
  | { status: 'pending'; label: string }
  | { status: 'error'; error: string };

const mutations = signal<ReadonlyMap<string, ResourceMutationState>>(new Map());
const idleMutation: ResourceMutationState = { status: 'idle' };

export function resourceMutationState(instanceId: string): ResourceMutationState {
  return mutations.value.get(instanceId) ?? idleMutation;
}

/** Owns only the in-flight UI intent. Backend admission remains authoritative. */
export async function runResourceMutation(
  instanceId: string,
  label: string,
  action: () => Promise<void>,
): Promise<void> {
  if (resourceMutationState(instanceId).status === 'pending') {
    toast('Wait for the current file action to finish.', 'info');
    return;
  }
  mutations.value = new Map(mutations.value).set(instanceId, { status: 'pending', label });
  try {
    await action();
    const next = new Map(mutations.value);
    next.delete(instanceId);
    mutations.value = next;
  } catch (err) {
    const error = `${label}: ${errMessage(err)}`;
    mutations.value = new Map(mutations.value).set(instanceId, { status: 'error', error });
    toast(error, 'error');
  }
}

export async function confirmDeleteItems({
  count,
  itemLabel,
  message,
}: {
  count: number;
  itemLabel: string;
  message: string;
}): Promise<boolean> {
  if (count <= 0) return false;
  const label = count === 1 ? itemLabel : `${itemLabel}s`;
  const choice = await showChoice<'delete'>(
    message,
    [{ value: 'delete', label: `Delete ${label}`, variant: 'danger' }],
    { title: count === 1 ? `Delete ${itemLabel}` : `Delete selected ${label}` },
  );
  return choice === 'delete';
}

export async function runBulkMutation<T>({
  items,
  action,
  success,
  partial,
  onDone,
  onFailure,
}: {
  items: T[];
  action: (item: T) => Promise<void>;
  success: (count: number) => string;
  partial: (done: number, total: number, err: unknown) => string;
  onDone: () => void;
  onFailure?: () => void;
}): Promise<void> {
  if (items.length === 0) return;
  let done = 0;
  try {
    for (const item of items) {
      await action(item);
      done += 1;
    }
  } catch (err) {
    onFailure?.();
    throw new Error(partial(done, items.length, err));
  }
  toast(success(done));
  onDone();
}

export function partialFailureMessage(action: string, done: number, total: number, err: unknown): string {
  return `${action} confirmed for ${done} of ${total}. Refresh the list before trying again. Last error: ${errMessage(err)}`;
}
