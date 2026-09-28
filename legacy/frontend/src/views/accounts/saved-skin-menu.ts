import type { ContextMenuItem } from '../../ui/ContextMenu';
import type { SavedSkinRecord } from './types';

export function menuItemsForSavedSkin({
  skin,
  applied,
  selectedPreviewEditing,
  skinActionsEnabled,
  applying,
  pendingActionBusy,
  queued,
  deleting,
  onView,
  onApply,
  onApplyNow,
  onCancelQueue,
  onEdit,
  onDownload,
  onDelete,
}: {
  skin: SavedSkinRecord;
  applied?: boolean;
  selectedPreviewEditing: boolean;
  skinActionsEnabled: boolean;
  applying: boolean;
  pendingActionBusy: boolean;
  queued: boolean;
  deleting: boolean;
  onView: () => void;
  onApply: () => void;
  onApplyNow: () => void;
  onCancelQueue: () => void;
  onEdit: () => void;
  onDownload: () => void;
  onDelete: () => void;
}): ContextMenuItem[] {
  const activeOnProfile = applied ?? Boolean(skin.applied_at);
  const items: ContextMenuItem[] = [];

  if (!deleting) {
    items.push({ icon: 'image', label: 'View', onSelect: onView });
  }
  if (queued) {
    if (skinActionsEnabled && !pendingActionBusy) {
      items.push({ icon: 'check', label: 'Apply now', onSelect: onApplyNow });
    }
    if (!pendingActionBusy) {
      items.push({ icon: 'x', label: 'Cancel queue', onSelect: onCancelQueue });
    }
  }
  if (skinActionsEnabled && !activeOnProfile && !applying && !queued) {
    items.push({ icon: 'check', label: 'Apply', onSelect: onApply });
  }
  if (!selectedPreviewEditing && !deleting && !applying) {
    items.push({ icon: 'edit', label: 'Edit', onSelect: onEdit });
  }
  if (!deleting) {
    items.push({ icon: 'download', label: 'Download PNG', onSelect: onDownload });
    if (!activeOnProfile) {
      items.push({ label: '', onSelect: () => {}, divider: true });
      items.push({ icon: 'trash', label: 'Delete', onSelect: onDelete, danger: true });
    }
  }

  return items;
}
