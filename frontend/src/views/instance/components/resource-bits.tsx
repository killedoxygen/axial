import type { JSX } from 'preact';
import { Button } from '../../../ui/Atoms';
import type { LoadState } from '../resources';
import { resourceMutationState } from '../bulk-actions';

/** What is being done to the instance's files, or why it was refused. A toolbar that says the first asks only for the refusal. */
export function ResourceMutationStatus({
  instanceId,
  refusalOnly,
}: {
  instanceId: string;
  refusalOnly?: boolean;
}): JSX.Element | null {
  const state = resourceMutationState(instanceId);
  if (state.status === 'idle' || (refusalOnly && state.status === 'pending')) return null;
  return (
    <div
      class={`cp-resource-note${state.status === 'error' ? ' cp-resource-note--error' : ''}`}
      role={state.status === 'error' ? 'alert' : 'status'}
    >
      {state.status === 'error' ? state.error : `${state.label}…`}
    </div>
  );
}

/** A toolbar's free space, which says what is being done to the files, so the rows under it never move for it. */
export function PaneDoing({ instanceId }: { instanceId: string }): JSX.Element {
  const state = resourceMutationState(instanceId);
  return (
    <span class="cp-pane-doing" role="status">
      {state.status === 'pending' && `${state.label}…`}
    </span>
  );
}

/** A listing that has nothing to show yet, or was refused. The wait is said only once it has lasted. */
export function ResourceStatus({
  state,
  noun = 'files',
  onRetry,
}: {
  state: LoadState<unknown>;
  noun?: string;
  onRetry: () => void;
}): JSX.Element | null {
  if (state.status === 'loading' && !state.data) {
    return <div class="cp-resource-note cp-resource-note--wait">Loading {noun}…</div>;
  }
  if (state.status === 'error') {
    return (
      <div class="cp-resource-note cp-resource-note--error" role="alert">
        <span>{state.error}</span>
        <Button variant="secondary" size="sm" icon="refresh" onClick={onRetry}>
          Retry
        </Button>
      </div>
    );
  }
  return null;
}
