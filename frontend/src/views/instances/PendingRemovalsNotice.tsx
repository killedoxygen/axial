import type { JSX } from 'preact';
import { useEffect } from 'preact/hooks';
import { Button } from '../../ui/Atoms';
import { instances } from '../../store';
import { checkInstanceDeletionFlow, recoverInstanceDeletionFlow } from '../instance/instance-actions';
import {
  deletionDiscoveryError,
  instanceDeletions,
  refreshPendingInstanceDeletions,
} from '../instance/deletions';

export function PendingRemovalsNotice(): JSX.Element | null {
  useEffect(() => {
    void refreshPendingInstanceDeletions().catch(() => undefined);
  }, []);
  const pending = instanceDeletions.value;
  const error = deletionDiscoveryError.value;
  if (pending.length === 0 && !error) return null;
  return (
    <div class="cp-notice" aria-live="polite">
      <div class="cp-notice-copy">
        <strong>Unfinished instance removals</strong>
        {error && <p>{error}</p>}
        {pending.map((row) => {
          const name = instances.value.find((instance) => instance.id === row.instance_id)?.name ?? row.instance_id;
          const canContinue = row.status === 'pending_restore' || row.status === 'cleanup_pending';
          return (
            <div key={row.operation_id} class="cp-notice-details">
              <p>
                {name}:{' '}
                {row.status === 'pending_restore'
                  ? 'Removal did not commit. The instance can be restored.'
                  : row.status === 'cleanup_pending'
                    ? 'Removal committed. File cleanup is unfinished.'
                    : row.status === 'aborted'
                      ? 'Removal aborted. The instance and its files were preserved.'
                      : 'The removal result is unconfirmed. Check its status before trying again.'}{' '}
                {row.intent === 'keep_files' ? 'Original choice: keep files.' : 'Original choice: delete files.'}
              </p>
              {row.error && <p>{row.error}</p>}
              <Button
                size="sm"
                variant="secondary"
                disabled={row.busy}
                onClick={() => void checkInstanceDeletionFlow(row)}
              >
                {row.busy ? 'Checking removal…' : 'Check status'}
              </Button>
              {canContinue && (
                <Button
                  size="sm"
                  variant="secondary"
                  disabled={row.busy}
                  onClick={() => void recoverInstanceDeletionFlow(row)}
                >
                  {row.status === 'pending_restore' ? 'Restore instance' : 'Finish cleanup'}
                </Button>
              )}
            </div>
          );
        })}
        {error && (
          <Button
            size="sm"
            variant="secondary"
            onClick={() => void refreshPendingInstanceDeletions().catch(() => undefined)}
          >
            Check unfinished removals
          </Button>
        )}
      </div>
    </div>
  );
}
