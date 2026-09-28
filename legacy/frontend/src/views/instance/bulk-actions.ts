import { showChoice } from '../../ui/Dialog';
import { toast } from '../../toast';
import { errMessage } from '../../utils';

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
}: {
  items: T[];
  action: (item: T) => Promise<void>;
  success: (count: number) => string;
  partial: (done: number, total: number, err: unknown) => string;
  onDone: () => void;
}): Promise<void> {
  if (items.length === 0) return;
  let done = 0;
  try {
    for (const item of items) {
      await action(item);
      done += 1;
    }
    toast(success(done));
    onDone();
  } catch (err) {
    toast(partial(done, items.length, err), 'error');
    onDone();
  }
}

export function partialFailureMessage(action: string, done: number, total: number, err: unknown): string {
  return `${action} ${done} of ${total}. Last error: ${errMessage(err)}`;
}
