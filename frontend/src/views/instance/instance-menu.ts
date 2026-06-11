import type { ContextMenuItem } from '../../ui/ContextMenu';
import { navigate } from '../../ui-state';
import type { EnrichedInstance } from '../../types';
import { deleteInstanceFlow, duplicateInstance, openInstanceFolder, renameInstance } from './instance-actions';

export function instanceMenuItems(inst: EnrichedInstance): ContextMenuItem[] {
  return [
    { icon: 'play', label: 'Open detail', onSelect: () => navigate({ name: 'instance', id: inst.id }) },
    { icon: 'folder', label: 'Open folder', onSelect: () => void openInstanceFolder(inst.id) },
    { icon: 'copy', label: 'Duplicate', onSelect: () => void duplicateInstance(inst) },
    { icon: 'edit', label: 'Rename', onSelect: () => void renameInstance(inst) },
    { label: '', onSelect: () => {}, divider: true },
    { icon: 'trash', label: 'Delete', onSelect: () => void deleteInstanceFlow(inst), danger: true },
  ];
}
