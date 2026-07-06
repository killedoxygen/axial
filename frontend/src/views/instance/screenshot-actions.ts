import { api, apiResourceUrl } from '../../api';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import { prompt } from '../../ui/Dialog';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import type { EnrichedInstance, InstanceScreenshot } from '../../types-instance';
import { openInstanceFolder } from './instance-actions';
import { confirmDeleteItems, partialFailureMessage, runBulkMutation } from './bulk-actions';

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

async function removeScreenshot(inst: EnrichedInstance, screenshotName: string): Promise<void> {
  const res: any = await api(
    'DELETE',
    `/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(screenshotName)}`,
  );
  if (res?.error) throw new Error(res.error);
}

export function screenshotFileUrl(inst: EnrichedInstance, name: string): string {
  return apiResourceUrl(`/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(name)}/file`);
}

export async function renameScreenshot(
  inst: EnrichedInstance,
  screenshotName: string,
  onDone: (newName: string) => void,
): Promise<void> {
  const next = await prompt('New name for this screenshot', screenshotName, {
    title: 'Rename screenshot',
    confirmText: 'Rename',
    validate: (value) => screenshotNameError(value, screenshotName),
  });
  const nextName = next ?? '';
  if (!nextName || nextName === screenshotName) return;
  try {
    const res: any = await api(
      'PUT',
      `/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(screenshotName)}`,
      { name: nextName },
    );
    if (res?.error) throw new Error(res.error);
    toast('Screenshot renamed');
    onDone(nextName);
  } catch (err) {
    toast(`Could not rename the screenshot: ${errMessage(err)}`, 'error');
  }
}

export async function deleteScreenshots(
  inst: EnrichedInstance,
  shots: InstanceScreenshot[],
  onDone: () => void,
): Promise<void> {
  const confirmed = await confirmDeleteItems({
    count: shots.length,
    itemLabel: 'screenshot',
    message:
      shots.length === 1
        ? `Delete "${shots[0]!.name}" from this instance. This removes the screenshot file from disk.`
        : `Delete ${shots.length} screenshots from this instance. This removes the selected screenshot files from disk.`,
  });
  if (!confirmed) return;
  await runBulkMutation({
    items: shots,
    action: (shot) => removeScreenshot(inst, shot.name),
    success: (count) => (count === 1 ? 'Screenshot deleted' : `${count} screenshots deleted`),
    partial: (done, total, err) => partialFailureMessage('Deleted', done, total, err),
    onDone,
  });
}

export function screenshotMenuItems({
  inst,
  shot,
  selectionItem,
  onView,
  onRefresh,
}: {
  inst: EnrichedInstance;
  shot: InstanceScreenshot;
  selectionItem: ContextMenuItem;
  onView: () => void;
  onRefresh: () => void;
}): ContextMenuItem[] {
  return [
    selectionItem,
    { divider: true, label: '', onSelect: () => undefined },
    { icon: 'image', label: 'View', onSelect: onView },
    { icon: 'edit', label: 'Rename', onSelect: () => void renameScreenshot(inst, shot.name, onRefresh) },
    {
      icon: 'folder',
      label: 'Open screenshots folder',
      onSelect: () => void openInstanceFolder(inst.id, 'screenshots'),
    },
    { divider: true, label: '', onSelect: () => undefined },
    { icon: 'trash', label: 'Delete', onSelect: () => void deleteScreenshots(inst, [shot], onRefresh), danger: true },
  ];
}
