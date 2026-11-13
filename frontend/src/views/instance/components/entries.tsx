import type { ComponentChildren, JSX, Ref } from 'preact';
import { memo } from 'preact/compat';
import { useCallback, useMemo, useRef, useState } from 'preact/hooks';
import { Icon, type IconName } from '../../../ui/Icons';
import { Chip, Empty, IconButton, Input } from '../../../ui/Atoms';
import { Art } from '../../../ui/Art';
import { openContextMenu, type ContextMenuItem } from '../../../ui/ContextMenu';
import { SelectionActionTray, SelectionCheckbox, type SelectionAction } from '../../../ui/SelectionActionTray';
import { selectionMenuItem, selectionToggleLabel, useSelection } from '../../../ui/selection';
import { settleChildren } from '../../../ui/motion';
import { resourceMutationState } from '../bulk-actions';
import type { LoadState } from '../resources';
import { versionText, type VersionLabel } from '../version-label';
import { PaneDoing, ResourceMutationStatus, ResourceStatus } from './resource-bits';

/** One row of a pane's list: a mod, a pack, a world. */
export interface EntryView {
  key: string;
  title: string;
  /** The file's own name, told when its title is pointed at. */
  fileName?: string;
  byline?: string;
  /** The row's figure: a version, a size. */
  num?: VersionLabel;
  artUrl?: string;
  icon: IconName;
  /** The modpack that placed this file. */
  pack?: string;
  disabled?: boolean;
  /** An unpacked folder rather than an archive. */
  folder?: boolean;
  /** The version an update leads to. */
  update?: VersionLabel;
}

function Version({ label }: { label: VersionLabel }): JSX.Element {
  return (
    <>
      {label.text}
      {label.pre && <span class="cp-entry-pre">{label.pre}</span>}
    </>
  );
}

/**
 * A list row: art (its selection mark on the corner on hover), name and a quiet second line, an update, a figure,
 * a menu. It is drawn again only when its own facts change, so a long list stays cheap to select in and to refresh.
 */
const Entry = memo(function Entry({
  view,
  selected,
  busy,
  onToggle,
  onMenu,
  onUpdate,
}: {
  view: EntryView;
  selected: boolean;
  busy: boolean;
  onToggle: (key: string) => void;
  onMenu: (event: MouseEvent, key: string) => void;
  onUpdate: (key: string) => void;
}): JSX.Element {
  const { num, update } = view;
  const menu = (event: MouseEvent): void => onMenu(event, view.key);
  return (
    <div class="cp-entry" data-disabled={view.disabled === true} data-selected={selected} onContextMenu={menu}>
      <span class="cp-lead">
        <Art url={view.artUrl} icon={view.icon} />
        <SelectionCheckbox
          selected={selected}
          label={selectionToggleLabel(selected, view.title)}
          onToggle={(event) => {
            event.stopPropagation();
            onToggle(view.key);
          }}
        />
      </span>
      <span class="cp-entry-id">
        <span class="cp-entry-title">
          <strong title={view.fileName}>{view.title}</strong>
          {view.disabled && <Chip>Disabled</Chip>}
          {view.folder && <Chip icon="folder">Folder</Chip>}
        </span>
        {view.byline && <span class="cp-entry-by">{view.byline}</span>}
      </span>
      {update ? (
        <button
          type="button"
          class="cp-btn cp-btn--sm cp-btn--soft cp-entry-update"
          title={`Update to ${versionText(update)}`}
          aria-label={`Update ${view.title} to ${versionText(update)}`}
          disabled={busy}
          onClick={(event) => {
            event.stopPropagation();
            onUpdate(view.key);
          }}
        >
          <Icon name="arrow-up" size={12} />
          <span>
            <Version label={update} />
          </span>
        </button>
      ) : (
        <span />
      )}
      <span class="cp-entry-num" title={num && versionText(num)}>
        {num && <Version label={num} />}
      </span>
      <IconButton icon="dots" size={28} tooltip="More" onClick={menu} />
    </div>
  );
});

/** The one action for every pending update, carrying how many. */
function UpdateAll({
  count,
  noun,
  note,
  disabled,
  onClick,
}: {
  count: number;
  noun: string;
  note?: string;
  disabled: boolean;
  onClick: () => void;
}): JSX.Element {
  return (
    <button
      type="button"
      class="cp-btn cp-btn--soft cp-update-all"
      title={note}
      aria-label={`Update ${count} ${noun}${count === 1 ? '' : 's'}`}
      disabled={disabled}
      onClick={onClick}
    >
      <Icon name="arrow-up" size={13} />
      <span>Update all</span>
      <span class="cp-update-all-n">{count}</span>
    </button>
  );
}

const titles = new Intl.Collator(undefined, { sensitivity: 'base', numeric: true });

/** Case-insensitive display order, falling back to the key for ties. A row keeps its place whatever is known of it. */
export function compareEntries(left: EntryView, right: EntryView): number {
  return titles.compare(left.title, right.title) || (left.key < right.key ? -1 : left.key > right.key ? 1 : 0);
}

/** The search box matches titles, filenames, second lines, figures and pack names. */
export function entryMatches(view: EntryView, query: string): boolean {
  const needle = query.trim().toLowerCase();
  return (
    !needle ||
    [view.title, view.fileName, view.byline, view.num?.text, view.pack].some((text) =>
      text?.toLowerCase().includes(needle),
    )
  );
}

/** Display name for a file without metadata: its name without archive or disabled suffixes. */
export function fileDisplayName(name: string): string {
  return name.replace(/\.disabled$/i, '').replace(/\.(jar|zip)$/i, '');
}

const entryKey = (view: EntryView): string => view.key;

/**
 * A pane that lists an instance's files: one toolbar line (search, a filter, the pending updates, then refresh,
 * the folder and the pane's own tools), the rows, and the tray for what is selected. Every such tab is this pane,
 * so they share one layout and nothing moves when the tab changes or its facts arrive.
 */
export function EntryPane({
  instanceId,
  noun,
  rows,
  state,
  expected,
  onRefresh,
  onOpenFolder,
  folderTip = 'Open folder',
  filter,
  shown,
  updates,
  tools,
  note,
  empty,
  menu,
  actions,
  surface,
  children,
}: {
  instanceId: string;
  /** What one row is: "mod", "world". */
  noun: string;
  /** Every row, in display order. */
  rows: EntryView[];
  state: LoadState<unknown>;
  /** How many rows the instance last counted. A pane expected to be empty waits without its toolbar. */
  expected?: number;
  onRefresh: () => void;
  onOpenFolder: () => void;
  folderTip?: string;
  /** A control beside the search box, and the rows it keeps. */
  filter?: JSX.Element;
  shown?: (view: EntryView) => boolean;
  updates?: { count: number; note?: string; all: () => void; one: (key: string) => void };
  /** The pane's own tools, after refresh and the folder. */
  tools?: ComponentChildren;
  /** A refusal that is not the listing's own. */
  note?: string;
  empty: { title: string; hint: ComponentChildren; actions: JSX.Element };
  menu: (key: string, selectionItem: ContextMenuItem) => ContextMenuItem[];
  actions: (selected: EntryView[], clear: () => void) => SelectionAction[];
  /** The pane's own element, for a pane that takes dropped files. */
  surface?: JSX.HTMLAttributes<HTMLDivElement> & { ref?: Ref<HTMLDivElement>; 'data-drop-state'?: string };
  children?: ComponentChildren;
}): JSX.Element {
  const [q, setQ] = useState('');
  const busy = resourceMutationState(instanceId).status === 'pending';
  const visible = useMemo(
    () => rows.filter((view) => entryMatches(view, q) && (!shown || shown(view))),
    [rows, q, shown],
  );
  const figures = useMemo(() => rows.some((view) => view.num), [rows]);
  const selection = useSelection(visible, entryKey);
  // Rows keep the same handlers for life; these read whatever the pane last rendered with.
  const latest = useRef({ menu, selection, updates });
  latest.current = { menu, selection, updates };
  const onToggle = useCallback((key: string) => latest.current.selection.toggle(key), []);
  const onUpdate = useCallback((key: string) => latest.current.updates?.one(key), []);
  const onMenu = useCallback(
    (event: MouseEvent, key: string) =>
      openContextMenu(event, latest.current.menu(key, selectionMenuItem(latest.current.selection, key))),
    [],
  );

  const settled = state.status === 'ready' || state.data !== null;
  const none = rows.length === 0;
  const waiting = none && !settled && state.status === 'loading';
  const isEmpty = none && settled && state.status !== 'error';

  return (
    <div class="cp-pane" {...surface}>
      {children}
      {!isEmpty && !(waiting && expected === 0) && (
        <div class="cp-pane-bar">
          <Input value={q} onChange={setQ} placeholder={`Search ${noun}s`} icon="search" />
          {filter}
          {updates && updates.count > 0 && (
            <UpdateAll count={updates.count} noun={noun} note={updates.note} disabled={busy} onClick={updates.all} />
          )}
          <PaneDoing instanceId={instanceId} />
          <IconButton icon="refresh" tooltip="Refresh" onClick={onRefresh} />
          <IconButton icon="folder" tooltip={folderTip} onClick={onOpenFolder} />
          {tools}
        </div>
      )}
      {(expected !== 0 || state.status === 'error') && (
        <ResourceStatus state={state} noun={`${noun}s`} onRetry={onRefresh} />
      )}
      <ResourceMutationStatus instanceId={instanceId} refusalOnly />
      {note && (
        <div class="cp-resource-note cp-resource-note--error" role="alert">
          {note}
        </div>
      )}
      {isEmpty ? (
        <Empty title={empty.title} actions={empty.actions}>
          {empty.hint}
        </Empty>
      ) : none ? null : visible.length === 0 ? (
        <Empty title={`No ${noun}s match`}>Try a different search or filter.</Empty>
      ) : (
        <div class="cp-entries" data-settles data-figures={figures} ref={(list) => settleChildren(list)}>
          {visible.map((view) => (
            <Entry
              key={view.key}
              view={view}
              selected={selection.isSelected(view.key)}
              busy={busy}
              onToggle={onToggle}
              onMenu={onMenu}
              onUpdate={onUpdate}
            />
          ))}
        </div>
      )}
      <SelectionActionTray
        selection={selection}
        itemLabel={noun}
        actions={actions(selection.selectedItems, selection.clear)}
      />
    </div>
  );
}
