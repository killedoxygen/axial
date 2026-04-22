import type { JSX } from 'preact';
import { Card, Meter, SectionHeading } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { useTheme } from '../../hooks/use-theme';
import { installQueue, installState } from '../../store';

export function DownloadsView(): JSX.Element {
  const theme = useTheme();
  const state = installState.value;
  const queue = installQueue.value;
  const hasActive = state.status === 'active';

  return (
    <div class="cp-view-page" style={{ gap: 20 }}>
      <div class="cp-page-header">
        <div>
          <h1>Downloads</h1>
          <div class="cp-page-sub">
            {hasActive ? '1 active task' : 'Nothing downloading'}
            {queue.length > 0 ? ` · ${queue.length} queued` : ''}
          </div>
        </div>
      </div>

      {hasActive ? (
        <Card>
          <SectionHeading eyebrow="In progress" title={state.versionId} />
          <div style={{ fontSize: 12, color: theme.n.textDim, marginBottom: 6 }}>{state.label}</div>
          <Meter value={state.pct} />
          <div style={{ fontSize: 11, color: theme.n.textMute, marginTop: 6, textAlign: 'right' }}>
            {Math.round(state.pct)}%
          </div>
        </Card>
      ) : (
        <Card padding={32}>
          <div class="cp-empty">
            <Icon name="download" size={36} color="var(--text-mute)" />
            <h2>Nothing downloading</h2>
            <p>Launch an instance that needs a download, or install a new Minecraft version, and it'll show up here.</p>
          </div>
        </Card>
      )}

      {queue.length > 0 && (
        <Card padding={10}>
          <div style={{ fontSize: 11, fontWeight: 600, textTransform: 'uppercase', letterSpacing: 0.8, color: theme.n.textMute, padding: '8px 10px' }}>
            Queue
          </div>
          {queue.map((item, i) => (
            <div key={item.versionId + i} style={{
              display: 'flex', alignItems: 'center', gap: 10,
              padding: '10px', borderTop: `1px solid ${theme.n.line}`,
            }}>
              <Icon name="clock" size={15} color="var(--text-mute)" />
              <span style={{ fontSize: 13, color: theme.n.text }}>{item.versionId}</span>
              {item.loader && <span style={{ fontSize: 11, color: theme.n.textMute }}>· {item.loader.componentId}</span>}
            </div>
          ))}
        </Card>
      )}
    </div>
  );
}
