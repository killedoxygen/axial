import type { JSX } from 'preact';
import { useCallback, useEffect, useMemo, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button } from '../../../ui/Atoms';
import { openContextMenu } from '../../../ui/ContextMenu';
import { SelectionActionPill, SelectionCheckbox } from '../../../ui/SelectionActionPill';
import { selectionMenuItem, selectionToggleLabel, useSelection } from '../../../ui/selection';
import type { EnrichedInstance, InstanceScreenshot } from '../../../types-instance';
import { fmtBytes, fmtDayLabel, fmtRelative } from '../format';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceEmpty, ResourceStatus } from '../components/resource-bits';
import { ScreenshotLightbox } from '../components/screenshot-lightbox';
import { deleteScreenshots, screenshotFileUrl, screenshotMenuItems } from '../screenshot-actions';

type ScreenshotSort = 'newest' | 'name' | 'size';

const SCREENSHOT_SORT_LABELS: Record<ScreenshotSort, string> = {
  newest: 'Newest',
  name: 'Name',
  size: 'Size',
};

type OptimisticScreenshotRename = {
  previousName: string;
  shot: InstanceScreenshot;
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
  const rawScreenshots = resources.data?.screenshots ?? [];
  const [sort, setSort] = useState<ScreenshotSort>('newest');
  const [viewer, setViewer] = useState<string>('');
  const [optimisticRename, setOptimisticRename] = useState<OptimisticScreenshotRename | null>(null);
  const screenshots = useMemo(() => {
    if (!optimisticRename) return rawScreenshots;
    if (rawScreenshots.some((shot) => shot.name === optimisticRename.shot.name)) return rawScreenshots;
    return rawScreenshots.map((shot) => (shot.name === optimisticRename.previousName ? optimisticRename.shot : shot));
  }, [rawScreenshots, optimisticRename]);
  const sortedScreenshots = useMemo(() => {
    const next = [...screenshots];
    next.sort((a, b) => {
      if (sort === 'name') return a.name.toLowerCase().localeCompare(b.name.toLowerCase());
      if (sort === 'size') return b.size - a.size || a.name.localeCompare(b.name);
      return b.modified_at.localeCompare(a.modified_at) || a.name.localeCompare(b.name);
    });
    return next;
  }, [screenshots, sort]);
  const groups = useMemo(() => {
    if (sort !== 'newest') return [{ key: 'all', label: '', shots: sortedScreenshots }];
    const byDay = new Map<string, { key: string; label: string; shots: InstanceScreenshot[] }>();
    for (const shot of sortedScreenshots) {
      const day = new Date(shot.modified_at);
      const key = Number.isNaN(day.getTime()) ? 'earlier' : `${day.getFullYear()}-${day.getMonth()}-${day.getDate()}`;
      let group = byDay.get(key);
      if (!group) {
        group = { key, label: fmtDayLabel(shot.modified_at), shots: [] };
        byDay.set(key, group);
      }
      group.shots.push(shot);
    }
    return [...byDay.values()];
  }, [sortedScreenshots, sort]);
  const totalBytes = useMemo(() => screenshots.reduce((sum, shot) => sum + shot.size, 0), [screenshots]);
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
  const handleLightboxRename = (shot: InstanceScreenshot, newName: string): void => {
    setOptimisticRename({ previousName: shot.name, shot: { ...shot, name: newName } });
    setViewer(newName);
    onRefresh();
  };

  useEffect(() => {
    if (!optimisticRename || resources.status === 'loading') return;
    const hasRenamedShot = rawScreenshots.some((shot) => shot.name === optimisticRename.shot.name);
    const hasPreviousShot = rawScreenshots.some((shot) => shot.name === optimisticRename.previousName);
    if (hasRenamedShot || resources.status === 'ready' || !hasPreviousShot) setOptimisticRename(null);
  }, [optimisticRename, rawScreenshots, resources.status]);

  useEffect(() => {
    if (!viewer || resources.status === 'loading') return;
    if (!screenshots.some((shot) => shot.name === viewer)) setViewer('');
  }, [screenshots, viewer, resources.status]);

  return (
    <div class="cp-instance-body">
      <div class="cp-resource-toolbar cp-screenshots-toolbar">
        <div class="cp-resource-toolbar-title">
          <strong>
            {screenshots.length} screenshot{screenshots.length === 1 ? '' : 's'}
          </strong>
          {totalBytes > 0 ? <span>{fmtBytes(totalBytes)}</span> : null}
        </div>
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
            variant="secondary"
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
        <div class="cp-shot-groups">
          {groups.map((group) => (
            <section class="cp-shot-group" key={group.key}>
              {group.label ? (
                <div class="cp-shot-group-head">
                  <span class="cp-shot-group-label">{group.label}</span>
                  <span class="cp-shot-group-count">{group.shots.length}</span>
                  <span class="cp-shot-group-rule" aria-hidden="true" />
                </div>
              ) : null}
              <div class="cp-screenshots-grid">
                {group.shots.map((shot) => (
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
                          <span>{fmtBytes(shot.size)}</span>
                          <span>{fmtRelative(shot.modified_at)}</span>
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
            </section>
          ))}
        </div>
      )}
      {viewer ? (
        <ScreenshotLightbox
          inst={inst}
          shots={sortedScreenshots}
          name={viewer}
          onSelect={setViewer}
          onClose={() => setViewer('')}
          onRename={handleLightboxRename}
          onRefresh={onRefresh}
        />
      ) : null}
      <SelectionActionPill
        selection={selection}
        itemLabel="screenshot"
        actions={[{ label: 'Delete', icon: 'trash', danger: true, onClick: () => void deleteSelected() }]}
      />
    </div>
  );
}
