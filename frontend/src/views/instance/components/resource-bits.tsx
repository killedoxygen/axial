import type { JSX } from 'preact';
import { Button } from '../../../ui/Atoms';
import type { ResourceLoadState } from '../resources';
import { resourceMutationState } from '../bulk-actions';

export function ResourceMutationStatus({ instanceId }: { instanceId: string }): JSX.Element | null {
  const state = resourceMutationState(instanceId);
  if (state.status === 'idle') return null;
  return (
    <div
      class={`cp-resource-note${state.status === 'error' ? ' cp-resource-note--error' : ''}`}
      role={state.status === 'error' ? 'alert' : 'status'}
    >
      {state.status === 'error' ? state.error : `${state.label}…`}
    </div>
  );
}

export function ResourceStatus({
  state,
  onRetry,
}: {
  state: ResourceLoadState;
  onRetry: () => void;
}): JSX.Element | null {
  if (state.status === 'loading' && !state.data) {
    return <div class="cp-resource-note">Loading files…</div>;
  }
  if (state.status === 'error') {
    return (
      <div class="cp-resource-note cp-resource-note--error">
        <span>{state.error}</span>
        <Button variant="secondary" size="sm" icon="refresh" onClick={onRetry}>
          Retry
        </Button>
      </div>
    );
  }
  return null;
}
