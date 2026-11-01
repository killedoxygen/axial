import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Icon, type IconName } from '../../../ui/Icons';
import { IconButton, Pill } from '../../../ui/Atoms';
import { SelectionCheckbox } from '../../../ui/SelectionActionTray';
import { selectionToggleLabel } from '../../../ui/selection';
import { formatBytes } from '../../../format';

/** One installed file in the Mods, Resource packs or Shaders list. */
export interface ContentRowView {
  key: string;
  title: string;
  fileName: string;
  /** Version and author details, shown on the second line. */
  details: string[];
  iconUrl?: string;
  fallbackIcon: IconName;
  managed: boolean;
  /** The modpack that placed this file. */
  pack?: string;
  enabled: boolean;
  size?: number;
  update?: { version: string; disabled: boolean; onClick: () => void };
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
  const details = [
    ...(view.size === undefined ? ['Unpacked folder'] : []),
    ...(view.pack ? [`From ${view.pack}`] : []),
    ...view.details,
  ];
  return (
    <div
      class="cp-content-row cp-selection-row"
      data-disabled={!view.enabled}
      data-selected={selected}
      onContextMenu={onMenu}
    >
      <SelectionCheckbox
        selected={selected}
        label={selectionToggleLabel(selected, view.title)}
        onToggle={(e) => {
          e.stopPropagation();
          onToggle();
        }}
      />
      <ContentFileIcon url={view.iconUrl} fallback={view.fallbackIcon} />
      <span class="cp-content-row-main">
        <span class="cp-content-row-title">
          <span class="cp-resource-name" title={view.fileName}>
            {view.title}
          </span>
          {!view.enabled && <Pill>Disabled</Pill>}
          {view.update && (
            <button
              type="button"
              class="cp-mod-update"
              title={`Update to ${view.update.version}`}
              disabled={view.update.disabled}
              onClick={(e) => {
                e.stopPropagation();
                view.update?.onClick();
              }}
            >
              <Icon name="arrow-up" size={11} stroke={2.4} />
              Update
            </button>
          )}
        </span>
        {details.length > 0 && <span class="cp-content-row-details">{details.join(' · ')}</span>}
      </span>
      <span class="cp-mod-source" data-provider={view.managed ? 'modrinth' : 'local'}>
        {view.managed ? 'Modrinth' : 'Local'}
      </span>
      <span class="cp-resource-meta">{view.size === undefined ? '' : formatBytes(view.size)}</span>
      <IconButton icon="dots" size={24} tooltip="More" onClick={onMenu} />
    </div>
  );
}

export function ContentFileIcon({ url, fallback }: { url?: string; fallback: IconName }): JSX.Element {
  const [failed, setFailed] = useState<string | null>(null);
  return (
    <span class="cp-content-row-icon">
      {url && failed !== url ? (
        <img src={url} alt="" loading="lazy" decoding="async" onError={() => setFailed(url)} />
      ) : (
        <Icon name={fallback} size={16} color="var(--text-mute)" />
      )}
    </span>
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
  return [view.title, view.fileName, view.pack ?? '', ...view.details].some((text) =>
    text.toLowerCase().includes(needle),
  );
}

/** Display name for a file without metadata: its name without archive or disabled suffixes. */
export function fileDisplayName(name: string): string {
  return name.replace(/\.disabled$/i, '').replace(/\.(jar|zip)$/i, '');
}
