import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Button, Card, IconButton, Meter, Pill, SectionHeading } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { useTheme } from '../../hooks/use-theme';
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
  const theme = useTheme();
  const state = installState.value;
  const queueState = installQueueState.value;
  const queue = queueState.items;
  const queueView = queueState.view_model;
  const failure = installFailure.value;
  const hasActive = state.status === 'active';
  const activeStartedAt = hasActive ? state.startedAt : 0;
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

  const activeTitle = hasActive ? state.displayName || state.versionId : '';
  const activePct = hasActive ? Math.round(Math.max(0, Math.min(100, state.pct))) : 0;
  const activeStep = hasActive ? state.activeStep : undefined;
  const stepPct = activeStep ? Math.round(Math.max(0, Math.min(100, activeStep.pct))) : 0;
  const stepRatio = activeStep ? activeStepRatio(activeStep.current, activeStep.total) : '';
  const nextQueuedLabel = queueView.next_label || '';
  const failureView = failure?.viewModel;
  const failureDetails = failureView?.details ?? [];
  const retryAction = failureView?.retry_action;
  const failureCard = failure ? (
    <Card>
      <SectionHeading
        title={failureView?.title || 'Install failed'}
        right={
          <Pill tone="err" icon="alert">
            Failed
          </Pill>
        }
      />
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, flexWrap: 'wrap' }}>
        <div style={{ minWidth: 0, flex: '1 1 260px' }}>
          <div
            style={{
              fontSize: 13,
              fontWeight: 600,
              color: theme.n.text,
              overflow: 'hidden',
              textOverflow: 'ellipsis',
              whiteSpace: 'nowrap',
            }}
          >
            {failure.displayName}
          </div>
          <div
            style={{ fontSize: 12, color: theme.n.textDim, marginTop: 4, lineHeight: 1.45, overflowWrap: 'anywhere' }}
          >
            {failureView?.summary || 'Install failed.'}
          </div>
          {failureView?.detail && (
            <div
              style={{ fontSize: 12, color: theme.n.textDim, marginTop: 4, lineHeight: 1.45, overflowWrap: 'anywhere' }}
            >
              {failureView.detail}
            </div>
          )}
          {failureDetails.length > 1 && (
            <ul
              style={{ margin: '6px 0 0 16px', padding: 0, color: theme.n.textMute, fontSize: 11.5, lineHeight: 1.4 }}
            >
              {failureDetails.slice(1).map((detail) => (
                <li key={detail}>{detail}</li>
              ))}
            </ul>
          )}
          <div style={{ fontSize: 11, color: theme.n.textMute, marginTop: 6 }}>
            Failed at {formatFailureTime(failure.failedAt)}
          </div>
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginLeft: 'auto' }}>
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
    </Card>
  ) : null;

  return (
    <div class="cp-view-page" style={{ gap: 20 }}>
      {hasActive ? (
        <Card>
          <SectionHeading
            title={activeTitle}
            right={
              <div
                style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap', justifyContent: 'flex-end' }}
              >
                {queueView.queued_count > 0 && <Pill icon="clock">{queueView.queued_count_label}</Pill>}
              </div>
            }
          />
          <div
            style={{
              fontSize: 12,
              color: theme.n.textDim,
              marginBottom: 8,
              lineHeight: 1.45,
              overflowWrap: 'anywhere',
            }}
          >
            {state.label}
          </div>
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
          <div
            style={{
              display: 'flex',
              justifyContent: 'space-between',
              gap: 12,
              marginTop: 7,
              color: theme.n.textMute,
              fontSize: 11,
              lineHeight: 1.35,
            }}
          >
            <span>{formatElapsedTime(state.startedAt, elapsedNow)}</span>
            <span style={{ fontVariantNumeric: 'tabular-nums' }}>
              {activeStep ? `${activeStep.label} ${stepPct}% · overall ${activePct}%` : `${activePct}%`}
            </span>
          </div>
          {nextQueuedLabel && (
            <div
              style={{
                fontSize: 11.5,
                color: theme.n.textMute,
                marginTop: 10,
                lineHeight: 1.4,
                overflowWrap: 'anywhere',
              }}
            >
              Next: {nextQueuedLabel}
            </div>
          )}
        </Card>
      ) : failureCard ? (
        failureCard
      ) : (
        <Card padding={32}>
          <div class="cp-empty">
            <Icon name="download" size={36} color="var(--text-mute)" />
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
          <div
            style={{
              fontSize: 11,
              fontWeight: 600,
              textTransform: 'uppercase',
              letterSpacing: 0,
              color: theme.n.textMute,
              padding: '8px 10px',
            }}
          >
            {queueView.section_title}
          </div>
          {queue.map((item, i) => {
            return (
              <div
                key={item.queue_id}
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  gap: 10,
                  padding: '10px',
                  borderTop: `1px solid ${theme.n.line}`,
                }}
              >
                <span style={{ width: 18, fontSize: 11, color: theme.n.textMute, fontVariantNumeric: 'tabular-nums' }}>
                  {item.position}
                </span>
                <div style={{ display: 'flex', alignItems: 'baseline', gap: 6, minWidth: 0, flex: 1 }}>
                  <span
                    style={{
                      fontSize: 13,
                      color: theme.n.text,
                      minWidth: 0,
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                      whiteSpace: 'nowrap',
                    }}
                  >
                    {item.label}
                  </span>
                  {item.install_item.loader && (
                    <span
                      style={{
                        fontSize: 11,
                        color: theme.n.textMute,
                        minWidth: 0,
                        overflow: 'hidden',
                        textOverflow: 'ellipsis',
                        whiteSpace: 'nowrap',
                      }}
                    >
                      · {item.install_item.version_id}
                    </span>
                  )}
                </div>
                <IconButton
                  icon="trash"
                  size={28}
                  danger
                  tooltip={item.remove_action.disabled_reason || item.remove_action.label}
                  onClick={() => void removeQueuedInstall(item.queue_id)}
                  disabled={!item.remove_action.enabled}
                />
              </div>
            );
          })}
        </Card>
      )}
    </div>
  );
}
