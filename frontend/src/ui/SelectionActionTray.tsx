import type { JSX } from 'preact';
import { Button } from './Atoms';
import { FloatingTray, FloatingTrayDivider, FloatingTrayLabel } from './FloatingTray';
import { Icon, type IconName } from './Icons';
import type { SelectionState } from './selection';

export interface SelectionAction {
  label: string;
  icon?: IconName;
  danger?: boolean;
  disabled?: boolean;
  onClick: () => void;
}

export function SelectionActionTray({
  shown,
  count,
  itemLabel = 'item',
  actions,
  onClear,
  onSelectAll,
  allSelected,
  ariaLabel,
  selection,
}: {
  shown?: boolean;
  count?: number;
  itemLabel?: string;
  actions: SelectionAction[];
  onClear?: () => void;
  onSelectAll?: () => void;
  allSelected?: boolean;
  ariaLabel?: string;
  selection?: Pick<SelectionState<any>, 'selectedCount' | 'allSelected' | 'selectAll' | 'clear'>;
}): JSX.Element | null {
  const effectiveCount = selection?.selectedCount ?? count ?? 0;
  const effectiveAllSelected = selection?.allSelected ?? allSelected;
  const clear = selection?.clear ?? onClear;
  const selectAll = selection?.selectAll ?? onSelectAll;
  if (!(shown ?? effectiveCount > 0) || effectiveCount <= 0) return null;
  if (!clear) return null;
  const noun = effectiveCount === 1 ? itemLabel : `${itemLabel}s`;

  return (
    <FloatingTray ariaLabel={ariaLabel ?? `${effectiveCount} selected ${noun}`} reserveSpace>
      <FloatingTrayLabel>
        {effectiveCount} {noun} selected
      </FloatingTrayLabel>
      <FloatingTrayDivider />
      {selectAll && !effectiveAllSelected && (
        <Button variant="ghost" size="sm" onClick={selectAll}>
          Select all
        </Button>
      )}
      <Button variant="ghost" size="sm" icon="x" onClick={clear}>
        Clear
      </Button>
      {actions.length > 0 && <FloatingTrayDivider />}
      {actions.map((action) => (
        <Button
          key={action.label}
          variant={action.danger ? 'danger' : 'secondary'}
          size="sm"
          icon={action.icon}
          disabled={action.disabled}
          onClick={action.onClick}
        >
          {action.label}
        </Button>
      ))}
    </FloatingTray>
  );
}

export function SelectionCheckbox({
  selected,
  label,
  onToggle,
  className,
}: {
  selected: boolean;
  label: string;
  onToggle: (e: MouseEvent) => void;
  className?: string;
}): JSX.Element {
  return (
    <button
      type="button"
      class={className ? `cp-select-check ${className}` : 'cp-select-check'}
      data-selected={selected}
      aria-pressed={selected}
      aria-label={label}
      onClick={onToggle}
    >
      <Icon name="check" size={14} stroke={2.2} />
    </button>
  );
}
