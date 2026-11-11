import type { ComponentChildren, JSX } from 'preact';
import { Button, Chip, Empty, Meter, Ring } from '../../ui/Atoms';
import { Face } from '../../ui/Face';
import { Icon } from '../../ui/Icons';
import { InstanceTile } from '../../ui/InstanceVisual';
import { api } from '../../api';
import { installQueueStateResponse } from '../../dto-install';
import { instances } from '../../store';
import { navigate, openCreate } from '../../ui-state';
import { errMessage, showError } from '../../utils';
import { useNowTicker } from '../../hooks/use-now';
import { KIND_ICON } from '../discover/shared';
import { loaderKeyFromComponentId } from '../create/defaults';
import { LoaderLogo } from '../create/loader-logos';
import { installItemFromQueuedViewModel } from '../../install-item';
import type { InstallItem } from '../../types-install';
import {
  activeDownload,
  activeInstallRetryPending,
  applyInstallQueueResponse,
  downloadFailures,
  reconcileUncertainMutation,
  downloadQueue,
  removeQueuedInstall,
  retryActiveInstall,
  retryFailedInstall,
  type DownloadFailure,
} from '../../machines/downloads';

/** Dismissal belongs to the queue, so it holds across pages, clients and reloads. */
async function dismissFailure(failure: DownloadFailure): Promise<void> {
  const action = failure.viewModel.dismiss_action;
  if (action?.action !== 'dismiss' || action.enabled !== true) return;
  try {
    const path = `/install/queue/failures/${encodeURIComponent(failure.operationId)}`;
    await applyInstallQueueResponse(installQueueStateResponse(await api('DELETE', path)));
  } catch (error: unknown) {
    showError('Could not dismiss the failure: ' + errMessage(error));
    await reconcileUncertainMutation();
  }
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

function formatFailureTime(timestamp: number): string {
  return new Date(timestamp).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
}

function stepRatio(current: number | undefined, total: number | undefined): string {
  if (typeof current !== 'number' || typeof total !== 'number' || total <= 0) return '';
  return `${current}/${total}`;
}

/** What is being installed: the loader's mark for a game install, the content kind for content. */
function Lead({ item }: { item: InstallItem }): JSX.Element {
  const content = item.content?.action;
  let mark: JSX.Element;
  if (!content) {
    mark = <LoaderLogo loader={loaderKeyFromComponentId(item.loader?.componentId)} size={18} class="cp-dl-logo" />;
  } else if (content.kind === 'uninstall') {
    mark = <Icon name="trash" size={16} />;
  } else {
    const kinds = content.kind === 'modpack' ? ['modpack' as const] : content.selections.map((pick) => pick.kind);
    mark = (
      <Icon name={kinds.every((kind) => kind === kinds[0]) && kinds[0] ? KIND_ICON[kinds[0]] : 'download'} size={16} />
    );
  }
  return (
    <span class="cp-dl-lead" aria-hidden="true">
      {mark}
    </span>
  );
}

/** Where it goes: the target instance for content, the Minecraft version for a loader. */
function Target({ item }: { item: InstallItem }): JSX.Element | null {
  const instanceId = item.content?.instance_id;
  if (instanceId) {
    const instance = instances.value.find((entry) => entry.id === instanceId);
    return (
      <span class="cp-chip cp-dl-into">
        {instance && <InstanceTile inst={instance} />}
        {instance?.name ?? 'A removed instance'}
      </span>
    );
  }
  return item.loader ? <Chip mono>{item.loader.minecraftVersion}</Chip> : null;
}

function Row({
  item,
  title,
  children,
  end,
}: {
  item: InstallItem;
  title: string;
  children?: ComponentChildren;
  end: ComponentChildren;
}): JSX.Element {
  return (
    <div class="cp-row cp-dl-row">
      <Lead item={item} />
      <div class="cp-dl-id">
        <div class="cp-dl-top">
          <strong>{title}</strong>
          <Target item={item} />
        </div>
        {children}
      </div>
      <div class="cp-dl-end">{end}</div>
    </div>
  );
}

function FailureRow({ failure }: { failure: DownloadFailure }): JSX.Element {
  const view = failure.viewModel;
  const reasons = [view.summary || 'Install failed.', view.detail, ...view.details.slice(1)].filter(Boolean);
  const retry = view.retry_action;
  const dismiss = view.dismiss_action;
  return (
    <Row
      item={failure.item}
      title={failure.displayName || view.title || 'Install failed'}
      end={
        <>
          <span class="cp-dl-time">{formatFailureTime(failure.failedAt)}</span>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => retryFailedInstall(failure)}
            disabled={retry?.enabled !== true}
            title={retry?.disabled_reason || undefined}
          >
            {retry?.label || 'Retry install'}
          </Button>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => void dismissFailure(failure)}
            disabled={dismiss?.enabled !== true}
            title={dismiss?.disabled_reason || undefined}
          >
            {dismiss?.label || 'Dismiss'}
          </Button>
        </>
      }
    >
      <div class="cp-dl-why" title={reasons.join(' ')}>
        <Face eyes="x" tone="err" size={24} />
        <span>{reasons.join(' ')}</span>
      </div>
    </Row>
  );
}

export function DownloadsView(): JSX.Element {
  const active = activeDownload.value;
  const { items: queue, view_model: queueView } = downloadQueue.value;
  const failures = downloadFailures.value;
  const now = useNowTicker(Boolean(active));

  const title = active ? active.displayName || active.item.versionId : '';
  const pct = active ? Math.round(active.pct) : 0;
  const step = active?.activeStep ?? null;
  const ratio = step ? stepRatio(step.current, step.total) : '';

  if (!active && failures.length === 0 && queue.length === 0) {
    return (
      <div class="cp-view-page cp-downloads cp-downloads--empty">
        <Empty
          title={queueView.empty_title}
          actions={
            <>
              <Button variant="secondary" icon="compass" onClick={() => navigate({ name: 'discover' })}>
                Browse Discover
              </Button>
              <Button variant="secondary" icon="plus" onClick={openCreate}>
                New instance
              </Button>
            </>
          }
        />
      </div>
    );
  }

  return (
    <div class="cp-view-page cp-downloads">
      {active && (
        <section class="cp-dl-now" aria-label="Installing now" aria-live="polite">
          <div class="cp-dl-now-row">
            <Lead item={active.item} />
            <div class="cp-dl-id">
              <div class="cp-dl-top">
                <strong>{title}</strong>
                <Target item={active.item} />
              </div>
              <div class="cp-dl-step">
                <Ring progress={step ? step.progress_pct : null} size={14} />
                <span>{step?.label ?? active.label}</span>
                {ratio && <Chip mono>{ratio}</Chip>}
                {active.startedAt !== null && (
                  <span class="cp-dl-elapsed">{formatElapsedTime(active.startedAt, now)}</span>
                )}
              </div>
            </div>
            {active.installId && active.retryAction?.action === 'retry' && (
              <Button
                variant="secondary"
                size="sm"
                disabled={!active.retryAction.enabled || activeInstallRetryPending.value}
                title={active.retryAction.disabled_reason || undefined}
                onClick={() => void retryActiveInstall(active.installId)}
              >
                {active.retryAction.label}
              </Button>
            )}
            <span class="cp-dl-pct">
              {pct}
              <small>%</small>
            </span>
          </div>
          <Meter value={pct} height={6} ariaLabel={`Install progress for ${title}`} />
        </section>
      )}

      {failures.length > 0 && (
        <section class="cp-dl-group">
          <h2>Needs attention</h2>
          {failures.map((failure) => (
            <FailureRow key={failure.operationId} failure={failure} />
          ))}
        </section>
      )}

      {queue.length > 0 && (
        <section class="cp-dl-group">
          <h2>{queueView.section_title}</h2>
          {queue.map((item) => (
            <Row
              key={item.queue_id}
              item={installItemFromQueuedViewModel(item)}
              title={item.label}
              end={
                <button
                  type="button"
                  class="cp-btn cp-btn--sm cp-btn--ghost"
                  aria-label={item.remove_action.label}
                  title={item.remove_action.disabled_reason || undefined}
                  disabled={!item.remove_action.enabled}
                  onClick={() => void removeQueuedInstall(item.queue_id)}
                >
                  <span>Remove</span>
                </button>
              }
            />
          ))}
        </section>
      )}
    </div>
  );
}
