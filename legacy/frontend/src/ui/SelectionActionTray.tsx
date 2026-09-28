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
  itemLabel = 'item',
  actions,
  ariaLabel,
  selection,
}: {
  itemLabel?: string;
  actions: SelectionAction[];
  ariaLabel?: string;
  selection: Pick<SelectionState<unknown>, 'selectedCount' | 'allSelected' | 'selectAll' | 'clear'>;
}): JSX.Element | null {
  const { selectedCount, allSelected, selectAll, clear } = selection;
  if (selectedCount <= 0) return null;
  const noun = selectedCount === 1 ? itemLabel : `${itemLabel}s`;

  return (
    <FloatingTray ariaLabel={ariaLabel ?? `${selectedCount} selected ${noun}`} reserveSpace>
      <FloatingTrayLabel>
        {selectedCount} {noun} selected
      </FloatingTrayLabel>
      <FloatingTrayDivider />
      {!allSelected && (
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
