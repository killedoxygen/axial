import type { JSX } from 'preact';
import { useCallback, useEffect, useMemo, useState } from 'preact/hooks';
import { Button, Chip, Empty } from '../../../ui/Atoms';
import { RowId, RowMenu } from '../../../ui/Rows';
import { Segmented } from '../../../ui/Segmented';
import { openContextMenu } from '../../../ui/ContextMenu';
import { SelectionActionTray, SelectionCheckbox } from '../../../ui/SelectionActionTray';
import { selectionMenuItem, selectionToggleLabel, useSelection } from '../../../ui/selection';
import { formatBytes, fmtDayLabel, fmtRelative } from '../../../format';
import type { EnrichedInstance, InstanceScreenshot } from '../../../types-instance';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { Pane, PaneBar } from '../components/pane';
import { ScreenshotLightbox } from '../components/screenshot-lightbox';
import { deleteScreenshots, screenshotFileUrl, screenshotMenuItems } from '../screenshot-actions';
import { resourceMutationState } from '../bulk-actions';

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
  const busy = resourceMutationState(inst.id).status === 'pending';
  const [sort, setSort] = useState<ScreenshotSort>('newest');
  const [viewer, setViewer] = useState<{ name: string } | null>(null);
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
      onView: () => setViewer({ name: shot.name }),
      onRefresh,
    });
  const deleteSelected = async (): Promise<void> => {
    await deleteScreenshots(inst, selection.selectedItems, clearAndRefresh, onRefresh);
  };
  const clearAndRefresh = (): void => {
    selection.clear();
    onRefresh();
  };
  const handleLightboxRename = (shot: InstanceScreenshot, newName: string): void => {
    setOptimisticRename({ previousName: shot.name, shot: { ...shot, name: newName } });
    setViewer((current) => (current === viewer && current?.name === shot.name ? { name: newName } : current));
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
    if (!screenshots.some((shot) => shot.name === viewer.name)) setViewer(null);
  }, [screenshots, viewer, resources.status]);

  const empty = screenshots.length === 0 && resources.status === 'ready';
  const openFolder = (): void => void openInstanceFolder(inst.id, 'screenshots');

  return (
    <Pane
      instanceId={inst.id}
      bar={
        !empty && (
          <PaneBar
            instanceId={inst.id}
            fact={totalBytes > 0 && <Chip tone="bare">{formatBytes(totalBytes)}</Chip>}
            onRefresh={onRefresh}
            onOpenFolder={openFolder}
            folderTip="Open screenshots"
          >
            <Segmented<ScreenshotSort>
              role="tablist"
              ariaLabel="Sort screenshots"
              value={sort}
              onChange={setSort}
              options={(Object.keys(SCREENSHOT_SORT_LABELS) as ScreenshotSort[]).map((item) => ({
                value: item,
                label: SCREENSHOT_SORT_LABELS[item],
              }))}
            />
          </PaneBar>
        )
      }
      state={resources}
      onRetry={onRefresh}
    >
      {empty ? (
        <Empty
          title="No screenshots yet"
          actions={
            <Button variant="secondary" icon="folder" onClick={openFolder}>
              Open screenshots
            </Button>
          }
        >
          Minecraft saves screenshots here after you capture them in game.
        </Empty>
      ) : (
        <div class="cp-shot-groups">
          {groups.map((group) => (
            <section key={group.key}>
              {group.label ? (
                <div class="cp-head">
                  <h2>{group.label}</h2>
                  <Chip>{group.shots.length}</Chip>
                </div>
              ) : null}
              <div class="cp-shots">
                {group.shots.map((shot) => (
                  <div
                    class="cp-shot"
                    data-selected={selection.isSelected(shot.name)}
                    key={shot.name}
                    onContextMenu={(e) => openContextMenu(e, menuItems(shot))}
                  >
                    <button
                      class="cp-shot-thumb"
                      type="button"
                      aria-label={`View ${shot.name}`}
                      onClick={() => setViewer({ name: shot.name })}
                    >
                      <img src={screenshotFileUrl(inst, shot.name)} alt="" loading="lazy" />
                    </button>
                    <SelectionCheckbox
                      over
                      selected={selection.isSelected(shot.name)}
                      label={selectionToggleLabel(selection.isSelected(shot.name), shot.name)}
                      onToggle={(e) => {
                        e.stopPropagation();
                        selection.toggle(shot.name);
                      }}
                    />
                    <div class="cp-shot-caption">
                      <RowId
                        title={shot.name}
                        tip={shot.name}
                        byline={`${formatBytes(shot.size)}, ${fmtRelative(shot.modified_at)}`}
                      />
                      <RowMenu
                        label={`Screenshot actions for ${shot.name}`}
                        onOpen={(e) => openContextMenu(e, menuItems(shot))}
                      />
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
          name={viewer.name}
          onSelect={(name) => setViewer({ name })}
          onClose={() => setViewer(null)}
          onRename={handleLightboxRename}
          onRefresh={onRefresh}
        />
      ) : null}
      <SelectionActionTray
        selection={selection}
        itemLabel="screenshot"
        actions={[
          { label: 'Delete', icon: 'trash', danger: true, disabled: busy, onClick: () => void deleteSelected() },
        ]}
      />
    </Pane>
  );
}
