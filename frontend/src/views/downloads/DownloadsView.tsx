import type { JSX } from 'preact';
import type { InstallItem, LoaderComponentId } from '../../types';
import { Card, IconButton, Meter, Pill, SectionHeading } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { useTheme } from '../../hooks/use-theme';
import { installQueue, installState } from '../../store';
import { removeQueuedInstallAt } from '../../actions';

const QUEUED_LOADER_LABELS: Record<LoaderComponentId, string> = {
  'net.fabricmc.fabric-loader': 'Fabric',
  'org.quiltmc.quilt-loader': 'Quilt',
  'net.minecraftforge': 'Forge',
  'net.neoforged': 'NeoForge',
};

function formatQueuedLoaderLabel(loader: NonNullable<InstallItem['loader']>): string {
  const loaderName = QUEUED_LOADER_LABELS[loader.componentId];
  const loaderVersion = loader.loaderVersion.trim();
  const minecraftVersion = loader.minecraftVersion.trim();
  const label = loaderVersion ? `${loaderName} ${loaderVersion}` : `${loaderName} loader`;

  return minecraftVersion ? `${label} for Minecraft ${minecraftVersion}` : label;
}

export function DownloadsView(): JSX.Element {
  const theme = useTheme();
  const state = installState.value;
  const queue = installQueue.value;
  const hasActive = state.status === 'active';
  const queuedLabel = `${queue.length} queued`;
  const queuedItemLabel = queue.length === 1 ? '1 item queued' : `${queue.length} items queued`;
  const phaseLabel = hasActive && state.phase ? state.phase.replace(/_/g, ' ') : '';
  const pageStatus = hasActive
    ? `1 active task${queue.length > 0 ? ` · ${queuedLabel}` : ''}`
    : queue.length > 0
      ? `No active task · ${queuedLabel}`
      : 'Nothing downloading';

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
            title={state.versionId}
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

      {queue.length > 0 && (
        <Card padding={10}>
          <div style={{ fontSize: 11, fontWeight: 600, textTransform: 'uppercase', letterSpacing: 0, color: theme.n.textMute, padding: '8px 10px' }}>
            Queue
          </div>
          {queue.map((item, i) => (
            <div key={item.versionId + i} style={{
              display: 'flex', alignItems: 'center', gap: 10,
              padding: '10px', borderTop: `1px solid ${theme.n.line}`,
            }}>
              <span style={{ width: 18, fontSize: 11, color: theme.n.textMute, fontVariantNumeric: 'tabular-nums' }}>
                {i + 1}
              </span>
              <div style={{ display: 'flex', alignItems: 'baseline', gap: 6, minWidth: 0, flex: 1 }}>
                <span style={{ fontSize: 13, color: theme.n.text, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                  {item.versionId}
                </span>
                {item.loader && (
                  <span style={{ fontSize: 11, color: theme.n.textMute, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    · {formatQueuedLoaderLabel(item.loader)}
                  </span>
                )}
              </div>
              <IconButton
                icon="trash"
                size={28}
                danger
                tooltip={`Remove ${item.versionId} from queue`}
                onClick={() => removeQueuedInstallAt(i)}
              />
            </div>
          ))}
        </Card>
      )}
    </div>
  );
}
