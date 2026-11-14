import type { ComponentChildren, JSX } from 'preact';
import { IconButton } from './Atoms';
import { SelectionCheckbox } from './SelectionActionTray';
import { selectionToggleLabel } from './selection';

/* The parts every list row is made of. The row itself is a `.cp-row` or a table's `.cp-tr`; `atoms.css` holds how
   it lights and how these parts follow it. */

/** A row's first cell: its art, with the mark that selects it on the corner. */
export function RowLead({
  name,
  selected,
  onToggle,
  children,
}: {
  /** What the mark selects, for its label. */
  name: string;
  selected: boolean;
  onToggle: (event: MouseEvent) => void;
  children: ComponentChildren;
}): JSX.Element {
  return (
    <span class="cp-lead">
      {children}
      <SelectionCheckbox
        over
        selected={selected}
        label={selectionToggleLabel(selected, name)}
        onToggle={(event) => {
          event.stopPropagation();
          onToggle(event);
        }}
      />
    </span>
  );
}

/** What a row is: its name with the facts that qualify it, over one quiet line. */
export function RowId({
  title,
  tip,
  byline,
  children,
}: {
  title: string;
  /** Told when the name is pointed at: a file's own name, a name cut short. */
  tip?: string;
  byline?: ComponentChildren;
  /** Chips after the name. */
  children?: ComponentChildren;
}): JSX.Element {
  return (
    <span class="cp-row-id">
      <span class="cp-row-title">
        <strong title={tip}>{title}</strong>
        {children}
      </span>
      {byline != null && byline !== '' && <span class="cp-row-by">{byline}</span>}
    </span>
  );
}

/** A row's menu, quiet until the row is in hand. */
export function RowMenu({
  label = 'More',
  onOpen,
}: {
  label?: string;
  onOpen: (event: MouseEvent) => void;
}): JSX.Element {
  return (
    <IconButton
      class="cp-row-more"
      icon="dots"
      size={28}
      tooltip={label}
      onClick={(event) => {
        event.stopPropagation();
        onOpen(event);
      }}
    />
  );
}
