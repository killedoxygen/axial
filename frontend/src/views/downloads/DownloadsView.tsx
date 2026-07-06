import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import './downloads.css';
import { Button, Card, IconButton, Meter, Pill, SectionHeading } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { installFailure, installQueueState, installState } from '../../store';
import { clearInstallFailure } from '../../actions';
import { removeQueuedInstall, retryFailedInstall } from '../../install';

function formatFailureTime(timestamp: number): string {
  return new Date(timestamp).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

function formatElapsedTime(startedAt: number, now: number): string {
  const elapsedSeconds = Math.max(0, Math.floor((now - startedAt) / 1000));
  if (elapsedSeconds < 60) return `${elapsedSeconds}s elapsed`;
  const minutes = Math.floor(elapsedSeconds / 60);
  const seconds = elapsedSeconds % 60;
  if (minutes < 60) return `${minutes}m ${seconds.toString().padStart(2, '0')}s elapsed`;
  const hours = Math.floor(minutes / 60);
  const remainingMinutes = minutes % 60;
  return `${hours}h ${remainingMinutes.toString().padStart(2, '0')}m elapsed`;
}

function activeStepRatio(current: number | undefined, total: number | undefined): string {
  if (typeof current !== 'number' || typeof total !== 'number' || total <= 0) return '';
  return `${current}/${total}`;
}

export function DownloadsView(): JSX.Element {
  const state = installState.value;
  const queueState = installQueueState.value;
  const queue = queueState.items;
  const queueActive = queueState.active ?? null;
  const queueView = queueState.view_model;
  const failure = installFailure.value;
  const localActive = state.status === 'active' ? state : null;
  const hasActive = Boolean(queueActive || localActive);
  const activeStartedAt = localActive?.startedAt ?? 0;
  const [elapsedNow, setElapsedNow] = useState(() => Date.now());

  useEffect(() => {
    if (!hasActive) return;
    setElapsedNow(Date.now());
    const intervalId = window.setInterval(() => {
      setElapsedNow(Date.now());
    }, 1000);
    return () => {
      window.clearInterval(intervalId);
    };
  }, [hasActive, activeStartedAt]);

  const queueActiveStep = queueActive?.progress.active_step
    ? {
        phase: queueActive.progress.active_step.phase_id,
        label: queueActive.progress.active_step.label,
        pct: queueActive.progress.active_step.progress_pct,
        current: queueActive.progress.active_step.current,
        total: queueActive.progress.active_step.total,
      }
    : undefined;
  const activeTitle = queueActive?.label || localActive?.displayName || localActive?.versionId || '';
  const activeLabel = queueActive?.progress.label || localActive?.label || '';
  const activePct = Math.round(Math.max(0, Math.min(100, queueActive?.progress.progress_pct ?? localActive?.pct ?? 0)));
  const activeStep = queueActiveStep ?? localActive?.activeStep;
  const stepPct = activeStep ? Math.round(Math.max(0, Math.min(100, activeStep.pct))) : 0;
  const stepRatio = activeStep ? activeStepRatio(activeStep.current, activeStep.total) : '';
  const elapsedLabel = localActive
    ? formatElapsedTime(localActive.startedAt, elapsedNow)
    : queueActive?.summary || queueView.status_label;
  const nextQueuedLabel = queueView.next_label || '';
  const failureView = failure?.viewModel;
  const failureDetails = failureView?.details ?? [];
  const retryAction = failureView?.retry_action;
  const repairAction = failureView?.repair_action;
  const failureCard = failure ? (
    <div class="cp-notice cp-download-failure-notice" data-tone="error">
      <div class="cp-notice-mark">
        <Icon name="alert" size={16} />
      </div>
      <div class="cp-notice-copy">
        <div class="cp-download-failure-head">
          <strong>{failureView?.title || 'Install failed'}</strong>
          <Pill tone="err" icon="alert">
            Failed
          </Pill>
        </div>
        <div class="cp-download-failure-title">{failure.displayName}</div>
        <p>{failureView?.summary || 'Install failed.'}</p>
        {failureView?.detail && <p>{failureView.detail}</p>}
        {failureDetails.length > 1 && (
          <div class="cp-notice-details">
            <ul>
              {failureDetails.slice(1).map((detail) => (
                <li key={detail}>{detail}</li>
              ))}
            </ul>
          </div>
        )}
        <div class="cp-download-failure-time">Failed at {formatFailureTime(failure.failedAt)}</div>
      </div>
      <div class="cp-download-failure-actions">
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
          onClick={retryFailedInstall}
          disabled={retryAction ? !retryAction.enabled : false}
          title={retryAction?.disabled_reason || undefined}
        >
          {retryAction?.label || 'Retry install'}
        </Button>
        <IconButton
          icon="x"
          size={28}
          tooltip={failureView?.dismiss_action?.label || 'Dismiss failed install'}
          onClick={clearInstallFailure}
          disabled={failureView?.dismiss_action ? !failureView.dismiss_action.enabled : false}
        />
      </div>
    </div>
  ) : null;

  return (
    <div class="cp-view-page cp-downloads-page">
      {hasActive ? (
        <Card>
          <SectionHeading
            title={activeTitle}
            right={
              <div class="cp-download-heading-actions">
                {queueView.queued_count > 0 && <Pill icon="clock">{queueView.queued_count_label}</Pill>}
              </div>
            }
          />
          <div class="cp-download-active-label">{activeLabel}</div>
          <div class="cp-download-active-meter">
            <Meter value={activePct} ariaLabel={`Install progress for ${activeTitle}`} />
          </div>
          {activeStep && (
            <div class="cp-download-step">
              <div class="cp-download-step-head">
                <span>{activeStep.label}</span>
                <span>
                  {stepRatio ? `${stepRatio} · ` : ''}
                  {stepPct}%
                </span>
              </div>
              <div class="cp-download-active-meter cp-download-active-meter--step">
                <Meter value={stepPct} ariaLabel={`${activeStep.label} progress for ${activeTitle}`} />
              </div>
            </div>
          )}
          <div class="cp-download-active-footer">
            <span>{elapsedLabel}</span>
            <span class="cp-download-num">
              {activeStep ? `${activeStep.label} ${stepPct}% · overall ${activePct}%` : `${activePct}%`}
            </span>
          </div>
          {nextQueuedLabel && <div class="cp-download-next">Next: {nextQueuedLabel}</div>}
        </Card>
      ) : failureCard ? (
        failureCard
      ) : (
        <Card padding={32}>
          <div class="cp-empty">
            <span class="cp-download-empty-icon">
              <Icon name="download" size={36} />
            </span>
            {queue.length > 0 ? (
              <>
                <h2>{queueView.title}</h2>
                <p>{queueView.summary}</p>
              </>
            ) : (
              <>
                <h2>{queueView.empty_title}</h2>
                <p>{queueView.empty_summary}</p>
              </>
            )}
          </div>
        </Card>
      )}

      {hasActive && failureCard}

      {queue.length > 0 && (
        <Card padding={10}>
          <div class="cp-download-section-label">{queueView.section_title}</div>
          <div class="cp-table cp-download-queue-table">
            {queue.map((item) => {
              return (
                <div key={item.queue_id} class="cp-table-row cp-download-queue-row">
                  <span class="cp-table-cell cp-download-queue-position cp-download-num">{item.position}</span>
                  <div class="cp-table-cell cp-download-queue-main">
                    <span class="cp-table-row-title cp-download-queue-label">{item.label}</span>
                    {item.install_item.loader && (
                      <span class="cp-table-row-sub cp-download-queue-version">· {item.install_item.version_id}</span>
                    )}
                  </div>
                  <div class="cp-table-cell cp-download-queue-action">
                    <IconButton
                      icon="trash"
                      size={28}
                      danger
                      tooltip={item.remove_action.disabled_reason || item.remove_action.label}
                      onClick={() => void removeQueuedInstall(item.queue_id)}
                      disabled={!item.remove_action.enabled}
                    />
                  </div>
                </div>
              );
            })}
          </div>
        </Card>
      )}
    </div>
  );
}
