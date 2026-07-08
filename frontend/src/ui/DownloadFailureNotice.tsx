import type { ComponentChildren, JSX } from 'preact';
import './download-failure-notice.css';
import { Button, IconButton, Pill } from './Atoms';
import { Icon } from './Icons';
import type { DownloadFailure } from '../machines/downloads';

function formatFailureTime(timestamp: number): string {
  return new Date(timestamp).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

export function DownloadFailureNotice({
  failure,
  onRetry,
  onDismiss,
  trailing,
}: {
  failure: DownloadFailure;
  onRetry: () => void;
  onDismiss?: () => void;
  trailing?: ComponentChildren;
}): JSX.Element {
  const view = failure.viewModel;
  const extraDetails = view.details.length > 1 ? view.details.slice(1) : [];
  const retryAction = view.retry_action;
  const repairAction = view.repair_action;
  const dismissAction = view.dismiss_action;

  return (
    <div class="cp-notice cp-dlfail" data-tone="error" aria-live="polite">
      <div class="cp-notice-mark">
        <Icon name="alert" size={16} />
      </div>
      <div class="cp-notice-copy">
        <div class="cp-dlfail-head">
          <strong>{view.title || 'Install failed'}</strong>
          <Pill tone="err" icon="alert">
            Failed
          </Pill>
        </div>
        <div class="cp-dlfail-name">{failure.displayName}</div>
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
        <div class="cp-dlfail-time">Failed at {formatFailureTime(failure.failedAt)}</div>
      </div>
      <div class="cp-dlfail-actions">
        {repairAction && (
          <Button
            variant="secondary"
            size="sm"
            icon="shield-check"
            disabled={!repairAction.enabled}
            title={repairAction.disabled_reason || undefined}
          >
            {repairAction.label}
          </Button>
        )}
        <Button
          variant="secondary"
          size="sm"
          icon="refresh"
          onClick={onRetry}
          disabled={retryAction ? !retryAction.enabled : false}
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
            disabled={dismissAction ? !dismissAction.enabled : false}
          />
        )}
      </div>
    </div>
  );
}
