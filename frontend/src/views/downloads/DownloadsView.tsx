import type { JSX } from 'preact';
import { Button, Card, IconButton, Meter, Pill, SectionHeading } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { useTheme } from '../../hooks/use-theme';
import { installFailure, installQueue, installState } from '../../store';
import { clearInstallFailure, removeQueuedInstallAt } from '../../actions';
import { retryFailedInstall } from '../../install';
import { formatInstallItemLabel } from '../../install-labels';

function formatFailureTime(timestamp: number): string {
  return new Date(timestamp).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

export function DownloadsView(): JSX.Element {
  const theme = useTheme();
  const state = installState.value;
  const queue = installQueue.value;
  const failure = installFailure.value;
  const hasActive = state.status === 'active';
  const activeTitle = hasActive ? state.displayName || state.versionId : '';
  const queuedLabel = `${queue.length} queued`;
  const queuedItemLabel = queue.length === 1 ? '1 item queued' : `${queue.length} items queued`;
  const phaseLabel = hasActive && state.phase ? state.phase.replace(/_/g, ' ') : '';
  const pageStatus = hasActive
    ? `1 active task${queue.length > 0 ? ` · ${queuedLabel}` : ''}`
    : failure
      ? `Install failed${queue.length > 0 ? ` · ${queuedLabel}` : ''}`
    : queue.length > 0
      ? `No active task · ${queuedLabel}`
      : 'Nothing downloading';
  const failureCard = failure ? (
    <Card>
      <SectionHeading
        title="Install failed"
        right={<Pill tone="err" icon="alert">Failed</Pill>}
      />
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, flexWrap: 'wrap' }}>
        <div style={{ minWidth: 0, flex: '1 1 260px' }}>
          <div style={{
            fontSize: 13, fontWeight: 600, color: theme.n.text,
            overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
          }}>
            {failure.displayName}
          </div>
          <div style={{ fontSize: 12, color: theme.n.textDim, marginTop: 4, lineHeight: 1.45, overflowWrap: 'anywhere' }}>
            {failure.message}
          </div>
          <div style={{ fontSize: 11, color: theme.n.textMute, marginTop: 6 }}>
            Failed at {formatFailureTime(failure.failedAt)}
          </div>
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginLeft: 'auto' }}>
          <Button variant="secondary" size="sm" icon="refresh" onClick={retryFailedInstall}>Retry</Button>
          <IconButton
            icon="x"
            size={28}
            tooltip="Dismiss failed install"
            onClick={clearInstallFailure}
          />
        </div>
      </div>
    </Card>
  ) : null;

  return (
    <div class="cp-view-page" style={{ gap: 20 }}>
      <div class="cp-page-header">
        <div>
          <h1>Downloads</h1>
          <div class="cp-page-sub">{pageStatus}</div>
        </div>
      </div>

      {hasActive ? (
        <Card>
          <SectionHeading
            title={activeTitle}
            right={(
              <div style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap', justifyContent: 'flex-end' }}>
                {phaseLabel && <Pill>{phaseLabel}</Pill>}
                {queue.length > 0 && <Pill icon="clock">{queuedLabel}</Pill>}
              </div>
            )}
          />
          <div style={{ fontSize: 12, color: theme.n.textDim, marginBottom: 6 }}>{state.label}</div>
          <Meter value={state.pct} />
          <div style={{ fontSize: 11, color: theme.n.textMute, marginTop: 6, textAlign: 'right' }}>
            {Math.round(state.pct)}%
          </div>
        </Card>
      ) : failureCard ? (
        failureCard
      ) : (
        <Card padding={32}>
          <div class="cp-empty">
            <Icon name="download" size={36} color="var(--text-mute)" />
            {queue.length > 0 ? (
              <>
                <h2>Downloads queued</h2>
                <p>{queuedItemLabel} and waiting to start. The next item will begin automatically.</p>
              </>
            ) : (
              <>
                <h2>Nothing downloading</h2>
                <p>Launch an instance that needs a download, or install a new Minecraft version, and it'll show up here.</p>
              </>
            )}
          </div>
        </Card>
      )}

      {hasActive && failureCard}

      {queue.length > 0 && (
        <Card padding={10}>
          <div style={{ fontSize: 11, fontWeight: 600, textTransform: 'uppercase', letterSpacing: 0, color: theme.n.textMute, padding: '8px 10px' }}>
            Queue
          </div>
          {queue.map((item, i) => {
            const itemLabel = formatInstallItemLabel(item);
            return (
              <div key={item.versionId + i} style={{
                display: 'flex', alignItems: 'center', gap: 10,
                padding: '10px', borderTop: `1px solid ${theme.n.line}`,
              }}>
                <span style={{ width: 18, fontSize: 11, color: theme.n.textMute, fontVariantNumeric: 'tabular-nums' }}>
                  {i + 1}
                </span>
                <div style={{ display: 'flex', alignItems: 'baseline', gap: 6, minWidth: 0, flex: 1 }}>
                  <span style={{ fontSize: 13, color: theme.n.text, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {itemLabel}
                  </span>
                  {item.loader && (
                    <span style={{ fontSize: 11, color: theme.n.textMute, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                      · {item.versionId}
                    </span>
                  )}
                </div>
                <IconButton
                  icon="trash"
                  size={28}
                  danger
                  tooltip={`Remove ${itemLabel} from queue`}
                  onClick={() => removeQueuedInstallAt(i)}
                />
              </div>
            );
          })}
        </Card>
      )}
    </div>
  );
}
