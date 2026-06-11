import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Button } from '../../../ui/Atoms';
import { openContextMenu } from '../../../ui/ContextMenu';
import { prompt, showChoice } from '../../../ui/Dialog';
import { api, apiResourceUrl } from '../../../api';
import { toast } from '../../../toast';
import { errMessage } from '../../../utils';
import type { EnrichedInstance } from '../../../types';
import { fmtBytes, fmtRelative } from '../format';
import type { ResourceLoadState } from '../resources';
import { openInstanceFolder } from '../instance-actions';
import { ResourceEmpty, ResourceStatus } from '../components/resource-bits';

function screenshotKind(name: string): 'png' | 'jpeg' | 'webp' | '' {
  const lower = name.toLowerCase();
  if (lower.endsWith('.png')) return 'png';
  if (lower.endsWith('.jpg') || lower.endsWith('.jpeg')) return 'jpeg';
  if (lower.endsWith('.webp')) return 'webp';
  return '';
}

function screenshotNameError(value: string, currentName?: string): string | null {
  const name = value.trim();
  if (!name || name === '.' || name === '..') return 'Use a screenshot filename.';
  if (name !== value) return 'Screenshot names cannot start or end with spaces.';
  if (name.startsWith('.')) return 'Screenshot names cannot start with a dot.';
  if (/[\\/]/.test(name)) return 'Screenshot names cannot include folders.';
  if (/[\u0000-\u001f\u007f]/.test(name)) return 'Screenshot names cannot include control characters.';
  if (!/\.(png|jpe?g|webp)$/i.test(name)) return 'Use a PNG, JPG, JPEG, or WEBP filename.';
  if (currentName && screenshotKind(name) !== screenshotKind(currentName)) return 'Keep the same screenshot file type.';
  return null;
}

function screenshotFileUrl(inst: EnrichedInstance, name: string): string {
  return apiResourceUrl(`/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(name)}/file`);
}

async function renameScreenshot(inst: EnrichedInstance, screenshotName: string, onDone: () => void): Promise<void> {
  const next = await prompt('New name for this screenshot', screenshotName, {
    title: 'Rename screenshot',
    confirmText: 'Rename',
    validate: (value) => screenshotNameError(value, screenshotName),
  });
  const nextName = next ?? '';
  if (!nextName || nextName === screenshotName) return;
  try {
    const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(screenshotName)}`, { name: nextName });
    if (res?.error) throw new Error(res.error);
    toast('Screenshot renamed');
    onDone();
  } catch (err) {
    toast(`Could not rename the screenshot: ${errMessage(err)}`, 'error');
  }
}

async function deleteScreenshot(inst: EnrichedInstance, screenshotName: string, onDone: () => void): Promise<void> {
  const choice = await showChoice<'delete'>(
    `Delete "${screenshotName}" from this instance. This removes the screenshot file from disk.`,
    [{ value: 'delete', label: 'Delete screenshot', variant: 'danger' }],
    { title: 'Delete screenshot' },
  );
  if (choice !== 'delete') return;
  try {
    const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(screenshotName)}`);
    if (res?.error) throw new Error(res.error);
    toast('Screenshot deleted');
    onDone();
  } catch (err) {
    toast(`Could not delete the screenshot: ${errMessage(err)}`, 'error');
  }
}

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

  useEffect(() => {
    if (viewer && !screenshots.some((shot) => shot.name === viewer)) setViewer('');
  }, [screenshots, viewer]);

  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <div class="cp-resource-toolbar cp-screenshots-toolbar">
        <strong>{screenshots.length} screenshot{screenshots.length === 1 ? '' : 's'}</strong>
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
          <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>Refresh</Button>
          <Button variant="soft" size="sm" icon="folder" onClick={() => void openInstanceFolder(inst.id, 'screenshots')}>Open screenshots</Button>
        </div>
      </div>
      <ResourceStatus state={resources} onRetry={onRefresh} />
      {screenshots.length === 0 && resources.status !== 'loading' ? (
        <ResourceEmpty icon="image" title="No screenshots yet" hint="Minecraft saves screenshots here after you capture them in game." />
      ) : (
        <div class="cp-screenshots-grid">
          {sortedScreenshots.map((shot) => (
            <div
              class="cp-screenshot-tile"
              key={shot.name}
              onContextMenu={(e) => openContextMenu(e, [
                { icon: 'image', label: 'View', onSelect: () => setViewer(shot.name) },
                { icon: 'edit', label: 'Rename', onSelect: () => void renameScreenshot(inst, shot.name, onRefresh) },
                { icon: 'folder', label: 'Open screenshots folder', onSelect: () => void openInstanceFolder(inst.id, 'screenshots') },
                { divider: true, label: '', onSelect: () => undefined },
                { icon: 'trash', label: 'Delete', onSelect: () => void deleteScreenshot(inst, shot.name, onRefresh), danger: true },
              ])}
            >
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
                  <div class="cp-screenshot-name" title={shot.name}>{shot.name}</div>
                  <div class="cp-screenshot-meta">{fmtBytes(shot.size)} · {fmtRelative(shot.modified_at)}</div>
                </div>
                <button
                  class="cp-resource-action"
                  type="button"
                  aria-label={`Screenshot actions for ${shot.name}`}
                  onClick={(e) => openContextMenu(e, [
                    { icon: 'image', label: 'View', onSelect: () => setViewer(shot.name) },
                    { icon: 'edit', label: 'Rename', onSelect: () => void renameScreenshot(inst, shot.name, onRefresh) },
                    { divider: true, label: '', onSelect: () => undefined },
                    { icon: 'trash', label: 'Delete', onSelect: () => void deleteScreenshot(inst, shot.name, onRefresh), danger: true },
                  ])}
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
          onKeyDown={(e: KeyboardEvent) => { if (e.key === 'Escape') setViewer(''); }}
        >
          <div class="cp-screenshot-viewer-panel" onClick={(e) => e.stopPropagation()}>
            <div class="cp-screenshot-viewer-bar">
              <div>
                <strong title={viewedShot.name}>{viewedShot.name}</strong>
                <span>{fmtBytes(viewedShot.size)} · {fmtRelative(viewedShot.modified_at)}</span>
              </div>
              <button class="cp-resource-action" type="button" aria-label="Close screenshot viewer" onClick={() => setViewer('')}>
                <Icon name="x" size={15} />
              </button>
            </div>
            <img src={screenshotFileUrl(inst, viewedShot.name)} alt={viewedShot.name} />
          </div>
        </div>
      ) : null}
    </div>
  );
}
