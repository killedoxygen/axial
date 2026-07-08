import type { JSX } from 'preact';
import './downloads.css';
import { IconButton, Meter } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { DownloadFailureNotice } from '../../ui/DownloadFailureNotice';
import { useNowTicker } from '../../hooks/use-now';
import {
  activeDownload,
  clearDownloadFailure,
  downloadFailure,
  downloadQueue,
  removeQueuedInstall,
  retryFailedInstall,
} from '../../machines/downloads';

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

function stepRatio(current: number | undefined, total: number | undefined): string {
  if (typeof current !== 'number' || typeof total !== 'number' || total <= 0) return '';
  return `${current}/${total}`;
}

export function DownloadsView(): JSX.Element {
  const active = activeDownload.value;
  const { items: queue, view_model: queueView } = downloadQueue.value;
  const failure = downloadFailure.value;
  const now = useNowTicker(Boolean(active));

  const title = active ? active.displayName || active.item.versionId : '';
  const pct = active ? Math.round(Math.max(0, Math.min(100, active.pct))) : 0;
  const step = active?.activeStep ?? null;
  const stepPct = step ? Math.round(Math.max(0, Math.min(100, step.progress_pct))) : 0;
  const ratio = step ? stepRatio(step.current, step.total) : '';
  const headerSub = active
    ? `${queueView.status_label}${queueView.active_queued_count_label || ''}`
    : queueView.queued_count > 0
      ? queueView.queued_count_label
      : queueView.status_label;

  return (
    <div class="cp-view-page cp-downloads-page">
      <div class="cp-page-header">
        <div>
          <h1>Downloads</h1>
          <div class="cp-page-sub">{headerSub}</div>
        </div>
      </div>

      {failure && (
        <DownloadFailureNotice failure={failure} onRetry={retryFailedInstall} onDismiss={clearDownloadFailure} />
      )}

      {active ? (
        <section class="cp-dl-live" aria-live="polite">
          <div class="cp-dl-live-head">
            <span class="cp-dl-live-icon" aria-hidden="true">
              <Icon name="download" size={18} stroke={2} />
            </span>
            <div class="cp-dl-live-id">
              <h2>{title}</h2>
              <p>{active.label}</p>
            </div>
            <div class="cp-dl-live-readout">
              <span class="cp-dl-live-pct cp-dl-num">
                {pct}
                <em>%</em>
              </span>
              <span class="cp-dl-live-elapsed">{formatElapsedTime(active.startedAt, now)}</span>
            </div>
          </div>
          <Meter value={pct} height={6} ariaLabel={`Install progress for ${title}`} />
          {step && (
            <div class="cp-dl-live-step">
              <span class="cp-dl-live-step-label">{step.label}</span>
              <span class="cp-dl-live-step-count cp-dl-num">
                {ratio ? `${ratio} · ` : ''}
                {stepPct}%
              </span>
              <div class="cp-dl-live-step-meter">
                <Meter value={stepPct} height={3} ariaLabel={`${step.label} progress for ${title}`} />
              </div>
            </div>
          )}
          {queueView.next_label && (
            <div class="cp-dl-live-foot">
              <Icon name="clock" size={13} stroke={2} />
              <span>Next: {queueView.next_label}</span>
            </div>
          )}
        </section>
      ) : (
        !failure && (
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
        )
      )}

      {queue.length > 0 && (
        <section class="cp-dl-queue">
          <div class="cp-dl-queue-head">
            <span>{queueView.section_title}</span>
            <span class="cp-dl-queue-count cp-dl-num">{queueView.queued_count}</span>
          </div>
          <div class="cp-dl-queue-rows">
            {queue.map((item) => (
              <div key={item.queue_id} class="cp-dl-queue-row">
                <span class="cp-dl-queue-pos cp-dl-num">{item.position}</span>
                <div class="cp-dl-queue-main">
                  <span class="cp-dl-queue-label">{item.label}</span>
                  {item.install_item.loader && <span class="cp-dl-queue-version">{item.install_item.version_id}</span>}
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
            ))}
          </div>
        </section>
      )}
    </div>
  );
}
