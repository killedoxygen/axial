import type { JSX } from 'preact';
import { Button } from '../../ui/Atoms';
import { Art } from '../../ui/Art';
import { FloatingTray, FloatingTrayLabel } from '../../ui/FloatingTray';
import { plural } from '../../format';
import { navigate, openCreateDraft } from '../../ui-state';
import { InstallConflictSheet, useInstallFlow } from './install-flow';
import { InstanceTargetPicker, KIND_ICON } from './shared';
import { clearTray, targetInstance, tray, traySelections, unstage } from './state';

const PREVIEW_LIMIT = 5;

/** What has been staged, and where it can go: the target instance, an existing one, or a new one. */
export function Tray(): JSX.Element | null {
  const items = tray.value;
  const instance = targetInstance.value;
  const flow = useInstallFlow(instance?.id);
  if (items.length === 0) return null;

  const hidden = items.slice(PREVIEW_LIMIT);
  const addAll = async (): Promise<void> => {
    const outcome = await flow.add(traySelections(), plural(items.length, 'item', 'items'));
    if (outcome.status === 'queued') clearTray();
  };

  return (
    <>
      <FloatingTray class="cp-tray" ariaLabel={`${items.length} staged`} reserveSpace>
        <div class="cp-tray-items" role="list" aria-label="Staged content">
          {items.slice(0, PREVIEW_LIMIT).map((item) => (
            <button
              key={item.canonical_id}
              type="button"
              role="listitem"
              title={`${item.title}${item.version_label ? ` ${item.version_label}` : ''}. Click to unstage.`}
              aria-label={`Unstage ${item.title}`}
              disabled={flow.busy}
              onClick={() => unstage(item.canonical_id)}
            >
              <Art url={item.icon_url} icon={KIND_ICON[item.kind]} size={24} />
            </button>
          ))}
          {hidden.length > 0 && (
            <span
              class="cp-unit"
              role="listitem"
              title={hidden.map((item) => item.title).join(', ')}
              aria-label={`${hidden.length} more staged`}
            >
              +{hidden.length}
            </span>
          )}
        </div>
        <FloatingTrayLabel>{items.length} staged</FloatingTrayLabel>
        <Button variant="ghost" size="sm" onClick={clearTray} disabled={flow.busy}>
          Clear
        </Button>
        {instance ? (
          <Button size="sm" icon="download" busy={flow.busy} disabled={flow.busy} onClick={() => void addAll()}>
            Add to {instance.name}
          </Button>
        ) : (
          <>
            <InstanceTargetPicker
              placeholder="Add to existing…"
              onPick={(id) => navigate({ name: 'discover', target: id })}
            />
            <Button size="sm" icon="plus" onClick={() => openCreateDraft(tray.value)} disabled={flow.busy}>
              Create instance
            </Button>
          </>
        )}
      </FloatingTray>
      <InstallConflictSheet flow={flow} onQueued={clearTray} />
    </>
  );
}
