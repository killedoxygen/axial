import type { ComponentChildren, JSX } from 'preact';
import { Icon, type IconName } from '../../../ui/Icons';
import { Chip, IconButton } from '../../../ui/Atoms';
import { Art } from '../../../ui/Art';
import { SelectionCheckbox } from '../../../ui/SelectionActionTray';
import { selectionToggleLabel } from '../../../ui/selection';

/** One installed file in the Mods, Resource packs or Shaders list. */
export interface ContentRowView {
  key: string;
  title: string;
  fileName: string;
  /** Mods: the file's own version. */
  version?: string;
  /** Mods: the authors. Packs: the pack's description. */
  details: string[];
  iconUrl?: string;
  fallbackIcon: IconName;
  /** The modpack that placed this file. */
  pack?: string;
  enabled: boolean;
  /** An unpacked folder rather than an archive. */
  folder?: boolean;
  update?: { version: string; disabled: boolean; onClick: () => void };
}

/** A list row: art (its selection mark on hover), name and a quiet second line, a number, an action, a menu. */
export function Entry({
  art,
  title,
  titleTip,
  chips,
  byline,
  num,
  action,
  selected,
  disabled,
  onToggle,
  onMenu,
  menuLabel = 'More',
}: {
  art: JSX.Element;
  title: string;
  titleTip?: string;
  chips?: ComponentChildren;
  byline?: string;
  num?: string;
  action?: JSX.Element;
  selected: boolean;
  disabled?: boolean;
  onToggle: () => void;
  onMenu: (e: MouseEvent) => void;
  menuLabel?: string;
}): JSX.Element {
  return (
    <div class="cp-entry" data-disabled={disabled === true} data-selected={selected} onContextMenu={onMenu}>
      <span class="cp-lead">
        {art}
        <SelectionCheckbox
          selected={selected}
          label={selectionToggleLabel(selected, title)}
          onToggle={(e) => {
            e.stopPropagation();
            onToggle();
          }}
        />
      </span>
      <span class="cp-entry-id">
        <span class="cp-entry-title">
          <strong title={titleTip}>{title}</strong>
          {chips}
        </span>
        {byline && <span class="cp-entry-by">{byline}</span>}
      </span>
      <span class="cp-entry-num">{num}</span>
      {action ?? <span />}
      <IconButton icon="dots" size={28} tooltip={menuLabel} onClick={onMenu} />
    </div>
  );
}

export function ContentRow({
  view,
  selected,
  onToggle,
  onMenu,
}: {
  view: ContentRowView;
  selected: boolean;
  onToggle: () => void;
  onMenu: (e: MouseEvent) => void;
}): JSX.Element {
  const update = view.update;
  return (
    <Entry
      art={<Art url={view.iconUrl} icon={view.fallbackIcon} />}
      title={view.title}
      titleTip={view.fileName}
      chips={
        <>
          {!view.enabled && <Chip>Disabled</Chip>}
          {view.folder && <Chip icon="folder">Folder</Chip>}
        </>
      }
      byline={view.details.join(', ')}
      num={view.version}
      action={
        update && (
          <button
            type="button"
            class="cp-btn cp-btn--sm cp-btn--soft cp-entry-update"
            title={`Update to ${update.version}`}
            aria-label={`Update ${view.title} to ${update.version}`}
            disabled={update.disabled}
            onClick={(e) => {
              e.stopPropagation();
              update.onClick();
            }}
          >
            <Icon name="arrow-up" size={12} />
            <span>{update.version}</span>
          </button>
        )
      }
      selected={selected}
      disabled={!view.enabled}
      onToggle={onToggle}
      onMenu={onMenu}
    />
  );
}

/** The one action for every pending update, carrying how many. */
export function UpdateAll({
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

/** Case-insensitive display order, falling back to the filename for ties. */
export function compareContentRows(left: ContentRowView, right: ContentRowView): number {
  return (
    left.title.localeCompare(right.title, undefined, { sensitivity: 'base', numeric: true }) ||
    left.fileName.localeCompare(right.fileName)
  );
}

/** The search box matches titles, filenames, authors and pack names. */
export function contentRowMatches(view: ContentRowView, query: string): boolean {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;
  return [view.title, view.fileName, view.version ?? '', view.pack ?? '', ...view.details].some((text) =>
    text.toLowerCase().includes(needle),
  );
}

/** Display name for a file without metadata: its name without archive or disabled suffixes. */
export function fileDisplayName(name: string): string {
  return name.replace(/\.disabled$/i, '').replace(/\.(jar|zip)$/i, '');
}
