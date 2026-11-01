import type { ComponentChildren, JSX } from 'preact';
import { Button, IconButton } from './Atoms';
import { Icon } from './Icons';
import type { DownloadFailure } from '../machines/downloads';

function formatFailureTime(timestamp: number): string {
  return new Date(timestamp).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
}

export function DownloadFailureNotice({
  failure,
  instanceName,
  onRetry,
  onDismiss,
  trailing,
}: {
  failure: DownloadFailure;
  /** Where a content operation ran, named by the caller. */
  instanceName?: string;
  onRetry: () => void;
  onDismiss?: () => void;
  trailing?: ComponentChildren;
}): JSX.Element {
  const view = failure.viewModel;
  const extraDetails = view.details.length > 1 ? view.details.slice(1) : [];
  const retryAction = view.retry_action;
  const dismissAction = view.dismiss_action;

  return (
    <div class="cp-notice cp-dlfail" data-tone="error" aria-live="polite">
      <div class="cp-notice-mark">
        <Icon name="alert" size={16} />
      </div>
      <div class="cp-notice-copy">
        <div class="cp-dlfail-head">
          <strong>{failure.displayName || view.title || 'Install failed'}</strong>
        </div>
        <div class="cp-dlfail-time">
          {instanceName ? `Failed in ${instanceName} at ` : 'Failed at '}
          {formatFailureTime(failure.failedAt)}
        </div>
        <p>{view.summary || 'Install failed.'}</p>
        {view.detail && <p>{view.detail}</p>}
        {extraDetails.length > 0 && (
          <div class="cp-notice-details">
            <ul>
              {extraDetails.map((detail) => (
                <li key={detail}>{detail}</li>
              ))}
            </ul>
          </div>
        )}
      </div>
      <div class="cp-dlfail-actions">
        <Button
          variant="secondary"
          size="sm"
          icon="refresh"
          onClick={onRetry}
          disabled={retryAction?.enabled !== true}
          title={retryAction?.disabled_reason || undefined}
        >
          {retryAction?.label || 'Retry install'}
        </Button>
        {trailing}
        {onDismiss && (
          <IconButton
            icon="x"
            size={28}
            tooltip={dismissAction?.label || 'Dismiss failed install'}
            onClick={onDismiss}
            disabled={dismissAction?.enabled !== true}
          />
        )}
      </div>
    </div>
  );
}
