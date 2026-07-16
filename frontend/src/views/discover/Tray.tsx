import type { JSX } from 'preact';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { FloatingTray, FloatingTrayDivider, FloatingTrayLabel } from '../../ui/FloatingTray';
import { plural } from '../../format';
import { navigate, openCreateDraft } from '../../ui-state';
import { InstallConflictSheet, useInstallFlow } from './install-flow';
import { clearTray, targetInstance, tray, traySelections, unstage } from './state';
import { InstanceTargetPicker, KIND_ICON } from './shared';

const TRAY_PREVIEW_LIMIT = 5;

export function Tray(): JSX.Element | null {
  const items = tray.value;
  const visibleItems = items.slice(0, TRAY_PREVIEW_LIMIT);
  const hiddenItems = items.slice(TRAY_PREVIEW_LIMIT);
  const instance = targetInstance.value;
  const flow = useInstallFlow(instance?.id);

  if (items.length === 0) return null;

  const addAll = async (): Promise<void> => {
    const outcome = await flow.add(traySelections(), plural(items.length, 'item', 'items'));
    if (outcome.status === 'installed') clearTray();
  };

  return (
    <>
      <FloatingTray ariaLabel={`${items.length} staged`} reserveSpace>
        <div class="cp-tray-items" role="list" aria-label="Staged content">
          {visibleItems.map((item) => (
            <button
              key={item.canonical_id}
              type="button"
              class="cp-tray-item"
              role="listitem"
              title={`${item.title}${item.version_label ? ` ${item.version_label}` : ''}. Click to unstage.`}
              aria-label={`Unstage ${item.title}`}
              disabled={flow.busy}
              onClick={() => unstage(item.canonical_id)}
            >
              {item.icon_url ? (
                <img src={item.icon_url} alt="" loading="lazy" />
              ) : (
                <Icon name={KIND_ICON[item.kind]} size={13} />
              )}
              <span class="cp-tray-item-x" aria-hidden="true">
                <Icon name="x" size={11} stroke={2.4} />
              </span>
            </button>
          ))}
          {hiddenItems.length > 0 && (
            <span
              class="cp-tray-overflow"
              role="listitem"
              title={hiddenItems.map((item) => item.title).join(', ')}
              aria-label={`${hiddenItems.length} more staged items`}
            >
              +{hiddenItems.length}
            </span>
          )}
        </div>
        <FloatingTrayLabel>{plural(items.length, 'item', 'items')} staged</FloatingTrayLabel>
        <FloatingTrayDivider />
        <Button variant="ghost" size="sm" onClick={clearTray} disabled={flow.busy}>
          Clear
        </Button>
        <FloatingTrayDivider />
        {instance ? (
          <Button size="sm" icon="download" onClick={() => void addAll()} disabled={flow.busy}>
            {flow.busy ? 'Adding…' : `Add to ${instance.name}`}
          </Button>
        ) : (
          <>
            <InstanceTargetPicker
              placeholder="Add to existing…"
              width={180}
              onPick={(id) => navigate({ name: 'discover', target: id })}
            />
            <Button size="sm" icon="plus" onClick={() => openCreateDraft(tray.value)} disabled={flow.busy}>
              Create instance
            </Button>
          </>
        )}
      </FloatingTray>

      <InstallConflictSheet flow={flow} onInstalled={clearTray} />
    </>
  );
}
