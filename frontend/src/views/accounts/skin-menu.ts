import type { ContextMenuItem } from '../../ui/ContextMenu';

/** What can be done to a saved skin from its tile or from the stage. */
export function savedSkinMenuItems({
  worn,
  queued,
  editing,
  canWear,
  busy,
  onWear,
  onWearNow,
  onCancelQueue,
  onEdit,
  onDownload,
  onDelete,
}: {
  worn: boolean;
  queued: boolean;
  editing: boolean;
  /** The active account can change its Minecraft profile's skin. */
  canWear: boolean;
  busy: boolean;
  onWear: () => void;
  onWearNow: () => void;
  onCancelQueue: () => void;
  onEdit: () => void;
  onDownload: () => void;
  onDelete: () => void;
}): ContextMenuItem[] {
  const items: ContextMenuItem[] = [];
  if (queued && !busy) {
    if (canWear) items.push({ icon: 'check', label: 'Wear now', onSelect: onWearNow });
    items.push({ icon: 'x', label: 'Cancel queued change', onSelect: onCancelQueue });
  }
  if (canWear && !worn && !queued && !busy) items.push({ icon: 'shirt', label: 'Wear', onSelect: onWear });
  if (!editing && !busy) items.push({ icon: 'edit', label: 'Edit', onSelect: onEdit });
  items.push({ icon: 'download', label: 'Download PNG', onSelect: onDownload });
  if (!worn) {
    items.push(
      { label: '', onSelect: () => {}, divider: true },
      { icon: 'trash', label: 'Delete', onSelect: onDelete, danger: true },
    );
  }
  return items;
}
