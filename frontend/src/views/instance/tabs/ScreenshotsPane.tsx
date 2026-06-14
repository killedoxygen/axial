import type { JSX } from 'preact';
import { useCallback, useEffect, useMemo, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button } from '../../../ui/Atoms';
import { openContextMenu } from '../../../ui/ContextMenu';
import { SelectionActionPill, SelectionCheckbox } from '../../../ui/SelectionActionPill';
import { selectionMenuItem, selectionToggleLabel, useSelection } from '../../../ui/selection';
import type { EnrichedInstance, InstanceScreenshot } from '../../../types';
import { fmtBytes, fmtRelative } from '../format';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceEmpty, ResourceStatus } from '../components/resource-bits';
import { deleteScreenshots, screenshotFileUrl, screenshotMenuItems } from '../screenshot-actions';

type ScreenshotSort = 'newest' | 'name' | 'size';

const SCREENSHOT_SORT_LABELS: Record<ScreenshotSort, string> = {
  newest: 'Newest',
  name: 'Name',
  size: 'Size',
};

export function ScreenshotsPane({
  inst,
  resources,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  onRefresh: () => void;
}): JSX.Element {
  const screenshots = resources.data?.screenshots ?? [];
  const [sort, setSort] = useState<ScreenshotSort>('newest');
  const [viewer, setViewer] = useState<string>('');
  const sortedScreenshots = useMemo(() => {
    const next = [...screenshots];
    next.sort((a, b) => {
      if (sort === 'name') return a.name.toLowerCase().localeCompare(b.name.toLowerCase());
      if (sort === 'size') return b.size - a.size || a.name.localeCompare(b.name);
      return b.modified_at.localeCompare(a.modified_at) || a.name.localeCompare(b.name);
    });
    return next;
  }, [screenshots, sort]);
  const viewedShot = viewer ? screenshots.find((shot) => shot.name === viewer) : undefined;
  const selection = useSelection(
    sortedScreenshots,
    useCallback((shot: InstanceScreenshot) => shot.name, []),
  );
  const menuItems = (shot: InstanceScreenshot) =>
    screenshotMenuItems({
      inst,
      shot,
      selectionItem: selectionMenuItem(selection, shot.name),
      onView: () => setViewer(shot.name),
      onRefresh,
    });
  const deleteSelected = async (): Promise<void> => {
    await deleteScreenshots(inst, selection.selectedItems, clearAndRefresh);
  };
  const clearAndRefresh = (): void => {
    selection.clear();
    onRefresh();
  };

  useEffect(() => {
    if (viewer && !screenshots.some((shot) => shot.name === viewer)) setViewer('');
  }, [screenshots, viewer]);

  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <div class="cp-resource-toolbar cp-screenshots-toolbar">
        <strong>
          {screenshots.length} screenshot{screenshots.length === 1 ? '' : 's'}
        </strong>
        <div class="cp-screenshots-tools">
          <div class="cp-mini-seg" role="tablist" aria-label="Sort screenshots">
            {(Object.keys(SCREENSHOT_SORT_LABELS) as ScreenshotSort[]).map((item) => (
              <button
                key={item}
                type="button"
                role="tab"
                aria-selected={sort === item}
                data-active={sort === item}
                onClick={() => setSort(item)}
              >
                {SCREENSHOT_SORT_LABELS[item]}
              </button>
            ))}
          </div>
          <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>
            Refresh
          </Button>
          <Button
            variant="soft"
            size="sm"
            icon="folder"
            onClick={() => void openInstanceFolder(inst.id, 'screenshots')}
          >
            Open screenshots
          </Button>
        </div>
      </div>
      <ResourceStatus state={resources} onRetry={onRefresh} />
      {screenshots.length === 0 && resources.status !== 'loading' ? (
        <ResourceEmpty
          icon="image"
          title="No screenshots yet"
          hint="Minecraft saves screenshots here after you capture them in game."
        />
      ) : (
        <div class="cp-screenshots-grid">
          {sortedScreenshots.map((shot) => (
            <div
              class="cp-screenshot-tile"
              data-selected={selection.isSelected(shot.name)}
              key={shot.name}
              onContextMenu={(e) => openContextMenu(e, menuItems(shot))}
            >
              <SelectionCheckbox
                className="cp-screenshot-select"
                selected={selection.isSelected(shot.name)}
                label={selectionToggleLabel(selection.isSelected(shot.name), shot.name)}
                onToggle={(e) => {
                  e.stopPropagation();
                  selection.toggle(shot.name);
                }}
              />
              <button
                class="cp-screenshot-thumb"
                type="button"
                aria-label={`View ${shot.name}`}
                onClick={() => setViewer(shot.name)}
              >
                <img src={screenshotFileUrl(inst, shot.name)} alt="" loading="lazy" />
              </button>
              <div class="cp-screenshot-caption">
                <div class="cp-screenshot-text">
                  <div class="cp-screenshot-name" title={shot.name}>
                    {shot.name}
                  </div>
                  <div class="cp-screenshot-meta">
                    {fmtBytes(shot.size)} · {fmtRelative(shot.modified_at)}
                  </div>
                </div>
                <button
                  class="cp-resource-action"
                  type="button"
                  aria-label={`Screenshot actions for ${shot.name}`}
                  onClick={(e) => openContextMenu(e, menuItems(shot))}
                >
                  <Icon name="dots" size={15} />
                </button>
              </div>
            </div>
          ))}
        </div>
      )}
      {viewedShot ? (
        <div
          class="cp-screenshot-viewer"
          role="dialog"
          aria-modal="true"
          aria-label={viewedShot.name}
          onClick={() => setViewer('')}
          onKeyDown={(e: KeyboardEvent) => {
            if (e.key === 'Escape') setViewer('');
          }}
        >
          <div class="cp-screenshot-viewer-panel" onClick={(e) => e.stopPropagation()}>
            <div class="cp-screenshot-viewer-bar">
              <div>
                <strong title={viewedShot.name}>{viewedShot.name}</strong>
                <span>
                  {fmtBytes(viewedShot.size)} · {fmtRelative(viewedShot.modified_at)}
                </span>
              </div>
              <button
                class="cp-resource-action"
                type="button"
                aria-label="Close screenshot viewer"
                onClick={() => setViewer('')}
              >
                <Icon name="x" size={15} />
              </button>
            </div>
            <img src={screenshotFileUrl(inst, viewedShot.name)} alt={viewedShot.name} />
          </div>
        </div>
      ) : null}
      <SelectionActionPill
        selection={selection}
        itemLabel="screenshot"
        actions={[{ label: 'Delete', icon: 'trash', danger: true, onClick: () => void deleteSelected() }]}
      />
    </div>
  );
}
