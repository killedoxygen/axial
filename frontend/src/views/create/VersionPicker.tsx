import type { JSX } from 'preact';
import { useLayoutEffect, useRef } from 'preact/hooks';
import { Chip, Input, Ring } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Segmented } from '../../ui/Segmented';
import type { CreateDraftItem } from '../../ui-state';
import { CompatibilityPreview } from './CompatibilityPreview';
import type { CreateForm } from './use-create-form';
import { CHANNEL_LABEL, type VersionDownloadState, type VersionRowModel } from './view-model';
import type { Channel, LoaderKey } from './defaults';

const INSTALLED_LABEL: Record<Exclude<VersionDownloadState, 'none'>, string> = {
  full: 'Installed',
  base: 'Minecraft installed',
};

function installedTitle(state: VersionDownloadState, source: LoaderKey): string {
  return state === 'base' && source !== 'vanilla'
    ? 'Base Minecraft version is installed; loader still needs download'
    : 'Already installed';
}

/** A tag every listed row carries tells nothing apart; only the distinguishing ones are shown. */
function distinguishingTags(rows: VersionRowModel[]): (row: VersionRowModel) => VersionRowModel['tags'] {
  const counts = new Map<string, number>();
  for (const row of rows) for (const tag of row.tags) counts.set(tag.id, (counts.get(tag.id) ?? 0) + 1);
  return (row) => row.tags.filter((tag) => rows.length < 2 || (counts.get(tag.id) ?? 0) < rows.length);
}

/** The Minecraft version as one select. Open, the list unfolds under it with its filter and channels. */
export function VersionPicker({
  versions,
  sourceKey,
  draft,
  open,
  onOpenChange,
  filterRef,
}: {
  versions: CreateForm['versions'];
  sourceKey: LoaderKey;
  draft: CreateDraftItem[] | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  filterRef: { current: HTMLInputElement | null };
}): JSX.Element {
  const { rows, selected, loading } = versions;
  const listRef = useRef<HTMLUListElement | null>(null);
  const tagsOf = distinguishingTags(rows);
  const availableChannels = new Set(versions.availableChannels);

  useLayoutEffect(() => {
    if (listRef.current) listRef.current.scrollTop = 0;
  }, [sourceKey, versions.channel, versions.query]);

  return (
    <div class="cp-cr-versions">
      <button
        type="button"
        class="cp-select-trigger"
        aria-label="Minecraft version"
        aria-haspopup="listbox"
        aria-expanded={open}
        data-state={open ? 'open' : 'closed'}
        onClick={() => onOpenChange(!open)}
      >
        <span class="cp-select-value cp-chips">
          {selected ? (
            <>
              <span class="cp-cr-mono">{selected.displayName}</span>
              {tagsOf(selected).map((tag) => (
                <Chip key={tag.id}>{tag.label}</Chip>
              ))}
            </>
          ) : loading ? (
            <span class="cp-cr-pending">
              <Ring size={14} />
              Loading versions…
            </span>
          ) : (
            <span class="cp-select-placeholder">Choose a version</span>
          )}
        </span>
        <span class="cp-select-caret" aria-hidden="true">
          <Icon name={open ? 'chevron-up' : 'chevron-down'} size={14} stroke={2} />
        </span>
      </button>

      {open && (
        <div
          class="cp-pop cp-cr-versions-menu"
          onKeyDown={(event) => {
            if (event.key !== 'Escape') return;
            event.preventDefault();
            onOpenChange(false);
          }}
        >
          <div class="cp-cr-versions-bar">
            <Input
              value={versions.query}
              onChange={versions.setQuery}
              placeholder="Filter versions…"
              icon="search"
              inputRef={filterRef}
            />
            <Segmented<Channel>
              role="tablist"
              size="sm"
              ariaLabel="Release channel"
              value={versions.channel}
              onChange={versions.setChannel}
              options={versions.channelTabs.map((value) => {
                const available = availableChannels.has(value);
                return {
                  value,
                  label: CHANNEL_LABEL[value],
                  disabled: !available,
                  ...(available ? {} : { title: `No ${CHANNEL_LABEL[value].toLowerCase()} versions here` }),
                };
              })}
            />
          </div>
          {rows.length > 0 ? (
            <ul class="cp-cr-vlist" role="listbox" aria-label="Minecraft versions" ref={listRef}>
              {rows.map((row) => {
                const chosen = selected?.selectionId === row.selectionId;
                const candidate = versions.compatibility(row);
                return (
                  <li
                    key={row.selectionId}
                    class="cp-cr-vrow"
                    role="option"
                    aria-selected={chosen}
                    aria-disabled={!row.createEnabled}
                    title={row.disabledReason ?? undefined}
                    onClick={() => {
                      if (!row.createEnabled) return;
                      versions.choose(row.selectionId);
                      onOpenChange(false);
                    }}
                  >
                    <span class="cp-cr-mono">{row.displayName}</span>
                    {row.hint && <span class="cp-cr-vrow-hint">{row.hint}</span>}
                    {tagsOf(row).map((tag) => (
                      <Chip key={tag.id} title={tag.label}>
                        {tag.label}
                      </Chip>
                    ))}
                    {draft && candidate && <CompatibilityPreview candidate={candidate} draft={draft} />}
                    <span class="cp-head-fill" />
                    {row.downloadState !== 'none' && (
                      <Chip tone="bare" title={installedTitle(row.downloadState, sourceKey)}>
                        {INSTALLED_LABEL[row.downloadState]}
                      </Chip>
                    )}
                    <span class="cp-cr-vrow-mark" aria-hidden="true">
                      {chosen && <Icon name="check" size={13} stroke={2.6} />}
                    </span>
                  </li>
                );
              })}
            </ul>
          ) : (
            <p class="cp-cr-versions-none">{loading ? 'Loading versions…' : 'Nothing matches.'}</p>
          )}
        </div>
      )}
    </div>
  );
}
