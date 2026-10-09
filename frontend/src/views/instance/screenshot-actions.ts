import { api, apiResourceUrl } from '../../api';
import { toast } from '../../toast';
import { prompt } from '../../ui/Dialog';
import type { ContextMenuItem } from '../../ui/ContextMenu';
import type { EnrichedInstance, InstanceScreenshot } from '../../types-instance';
import { openInstanceFolder } from './instance-actions';
import { confirmDeleteItems, partialFailureMessage, runBulkMutation, runResourceMutation } from './bulk-actions';
import { dtoString } from '../../dto-contract';
import { requireResourceCommandSuccess } from './resources';

function screenshotKind(name: string): 'png' | 'jpeg' | 'webp' | '' {
  const lower = name.toLowerCase();
  if (lower.endsWith('.png')) return 'png';
  if (lower.endsWith('.jpg') || lower.endsWith('.jpeg')) return 'jpeg';
  if (lower.endsWith('.webp')) return 'webp';
  return '';
}

function screenshotNameError(value: string, currentName?: string): string | null {
  if (!value) return 'Use a screenshot filename.';
  if (!screenshotKind(value)) return 'Use a PNG, JPG, JPEG, or WEBP filename.';
  if (currentName && screenshotKind(value) !== screenshotKind(currentName)) {
    return 'Keep the same screenshot file type.';
  }
  return null;
}

async function removeScreenshot(inst: EnrichedInstance, screenshotName: string): Promise<void> {
  const res = await api(
    'DELETE',
    `/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(screenshotName)}`,
  );
  requireResourceCommandSuccess(res, 'Screenshot deletion');
}

export function screenshotFileUrl(inst: EnrichedInstance, name: string): string {
  return apiResourceUrl(`/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(name)}/file`);
}

export async function renameScreenshot(
  inst: EnrichedInstance,
  screenshotName: string,
  onDone: (newName: string) => void,
): Promise<void> {
  await runResourceMutation(inst.id, 'Rename screenshot', async () => {
    const next = await prompt('New name for this screenshot', screenshotName, {
      title: 'Rename screenshot',
      confirmText: 'Rename',
      validate: (value) => screenshotNameError(value, screenshotName),
    });
    const nextName = next ?? '';
    if (!nextName || nextName === screenshotName) return;
    const res = await api(
      'PUT',
      `/instances/${encodeURIComponent(inst.id)}/screenshots/${encodeURIComponent(screenshotName)}`,
      { name: nextName },
    );
    const renamed = dtoString(requireResourceCommandSuccess(res, 'Screenshot rename').name, 'Screenshot name');
    toast('Screenshot renamed');
    onDone(renamed);
  });
}

export async function deleteScreenshots(
  inst: EnrichedInstance,
  shots: InstanceScreenshot[],
  onDone: () => void,
  onFailure?: () => void,
): Promise<void> {
  if (shots.length === 0) return;
  await runResourceMutation(inst.id, 'Delete screenshots', async () => {
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
      partial: (done, total, err) => partialFailureMessage('Deletion', done, total, err),
      onDone,
      onFailure,
    });
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
    {
      icon: 'trash',
      label: 'Delete',
      onSelect: () => void deleteScreenshots(inst, [shot], onRefresh, onRefresh),
      danger: true,
    },
  ];
}
