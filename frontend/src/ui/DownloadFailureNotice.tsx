import type { ComponentChildren, JSX } from 'preact';
import { Button, Notice } from './Atoms';
import type { DownloadFailure } from '../machines/downloads';

function formatFailureTime(timestamp: number): string {
  return new Date(timestamp).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
}

export function DownloadFailureNotice({
  failure,
  onRetry,
  trailing,
}: {
  failure: DownloadFailure;
  onRetry: () => void;
  trailing?: ComponentChildren;
}): JSX.Element {
  const view = failure.viewModel;
  const extraDetails = view.details.slice(1);
  const retryAction = view.retry_action;

  return (
    <Notice tone="error" title={failure.displayName || view.title || 'Install failed'}>
      <p class="cp-dlfail-time">Failed at {formatFailureTime(failure.failedAt)}</p>
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
      </div>
    </Notice>
  );
}
