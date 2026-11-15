import type { JSX } from 'preact';
import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks';
import { Button, Chip, Ring } from '../ui/Atoms';
import { Icon } from '../ui/Icons';
import { Popover } from '../ui/Popover';
import { Segmented } from '../ui/Segmented';
import { quickCreate, type CreateDraftItem } from '../app/route';
import { CompatibilityPreview } from './CompatibilityPreview';
import { LoaderLogo } from './loader-logos';
import type { CreateForm } from './use-create-form';
import { CHANNEL_LABEL, loaderKeyFromSourceId, type VersionDownloadState, type VersionRowModel } from './versions';
import { defaultNameFor, type Channel } from './defaults';
import { caution, heed } from '../app/caution';
import { habit } from '../app/habits';

const INSTALLED_LABEL: Record<Exclude<VersionDownloadState, 'none'>, string> = {
  full: 'Installed',
  base: 'Minecraft installed',
};

function installedTitle(row: VersionRowModel): string {
  return row.downloadState === 'base' && row.sourceId !== 'vanilla'
    ? 'Base Minecraft version is installed; loader still needs download'
    : 'Already installed';
}

/* Someone filtering the list may press Enter to pick a version, not to make an instance of it. Whoever has created
   this way a few times knows what the key does. */
const enterCreates = caution<VersionRowModel>({
  id: 'create.enter-creates',
  area: 'create',
  applies: () => habit('create.quick') < 3,
  held: (row) => `${defaultNameFor(loaderKeyFromSourceId(row.sourceId), row.id)} selected`,
  why: 'Not created, in case you were only picking.',
  proceed: 'Create it',
});

const LIST_ID = 'cp-cr-version-list';
const rowId = (row: VersionRowModel): string => `cp-cr-version-${row.selectionId}`;

/**
 * The Minecraft version as one field that is also its search. Typing lists the closest versions across channels,
 * and in the loader its letters name (`forge 1.13.1`, `f1131`); Enter creates the top one.
 */
export function VersionPicker({
  versions,
  draft,
  open,
  onOpenChange,
  inputRef,
  onCreate,
}: {
  versions: CreateForm['versions'];
  draft: CreateDraftItem[] | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  inputRef: { current: HTMLInputElement | null };
  onCreate: () => void;
}): JSX.Element {
  const { rows, selected, loading, query, searching } = versions;
  const boxRef = useRef<HTMLDivElement | null>(null);
  const listRef = useRef<HTMLUListElement | null>(null);
  const availableChannels = new Set(versions.availableChannels);

  // The row in hand: the one the arrows or the pointer last reached in this listing, else the top match of a
  // search or the chosen version.
  const listing = `${query}\n${versions.channel}`;
  const [reached, setReached] = useState<{ listing: string; id: string } | null>(null);
  const hand =
    (reached?.listing === listing ? rows.find((row) => row.selectionId === reached.id) : undefined) ??
    (searching
      ? rows.find((row) => row.createEnabled)
      : rows.find((row) => row.selectionId === selected?.selectionId)) ??
    null;
  // Enter pressed on a search whose loaders were still being listed waits for the settled top match.
  const [enterOn, setEnterOn] = useState<string | null>(null);

  const close = (): void => {
    onOpenChange(false);
    versions.setQuery('');
  };
  const accept = (row: VersionRowModel | null, create: boolean): void => {
    if (!row?.createEnabled) return;
    if (create) versions.create(row);
    else versions.choose(row);
    onOpenChange(false);
  };

  // The notice that holds a creation back can outlive this render, and the dialog.
  const latest = useRef<((row: VersionRowModel) => void) | null>(null);
  latest.current = versions.create;
  useEffect(() => () => void (latest.current = null), []);

  /** Enter on a search: a named loader is created at once; a bare version is only selected, and says so. */
  const enter = (row: VersionRowModel | null): void => {
    if (!row?.createEnabled) return;
    const create = (): void => (latest.current ? latest.current(row) : quickCreate(row));
    accept(row, versions.loaderNamed || heed(enterCreates, row, create));
  };

  useEffect(() => {
    if (enterOn === null) return;
    if (enterOn !== query || !open) setEnterOn(null);
    else if (!versions.searchPending) {
      setEnterOn(null);
      enter(hand);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [enterOn, query, open, versions.searchPending]);

  useLayoutEffect(() => {
    if (listRef.current) listRef.current.scrollTop = 0;
  }, [versions.channel, query]);
  useLayoutEffect(() => {
    if (open && hand) document.getElementById(rowId(hand))?.scrollIntoView({ block: 'nearest' });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, hand?.selectionId]);

  const step = (by: 1 | -1): void => {
    if (rows.length === 0) return;
    const from = hand ? rows.indexOf(hand) : by === 1 ? -1 : rows.length;
    const next = rows[Math.max(0, Math.min(rows.length - 1, from + by))];
    if (next) setReached({ listing, id: next.selectionId });
  };

  const onKeyDown = (event: KeyboardEvent): void => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      if (open) step(event.key === 'ArrowDown' ? 1 : -1);
      else onOpenChange(true);
    } else if (event.key === 'Enter' && !event.ctrlKey) {
      event.preventDefault();
      if (!open) onCreate();
      else if (!searching) {
        if (hand) accept(hand, false);
        else close();
      } else if (versions.searchPending) setEnterOn(query);
      else enter(hand);
    } else if (event.key === 'Tab' && open) {
      // Tab takes the match and moves on to the rest of the form.
      if (searching && hand?.createEnabled) accept(hand, false);
      else close();
    }
  };

  return (
    <div class="cp-cr-versions">
      <div
        ref={boxRef}
        class="cp-select-trigger cp-cr-combo"
        data-state={open ? 'open' : 'closed'}
        onClick={() => {
          inputRef.current?.focus();
          if (!open) onOpenChange(true);
        }}
      >
        <input
          ref={inputRef}
          class="cp-cr-version"
          type="text"
          role="combobox"
          data-autofocus
          aria-label="Minecraft version"
          aria-haspopup="listbox"
          aria-expanded={open}
          aria-controls={LIST_ID}
          aria-activedescendant={open && hand ? rowId(hand) : undefined}
          aria-busy={enterOn !== null || undefined}
          autocomplete="off"
          spellcheck={false}
          value={query}
          placeholder={
            open ? 'Search versions' : (selected?.displayName ?? (loading ? 'Loading versions…' : 'Choose a version'))
          }
          data-chosen={selected ? '' : undefined}
          onInput={(event) => {
            versions.setQuery(event.currentTarget.value);
            if (!open) onOpenChange(true);
          }}
          onKeyDown={onKeyDown}
        />
        {((!selected && loading) || versions.searchPending || enterOn !== null) && <Ring size={14} />}
        <button
          type="button"
          class="cp-select-caret"
          tabIndex={-1}
          aria-label={open ? 'Close versions' : 'Show versions'}
          onClick={(event) => {
            event.stopPropagation();
            inputRef.current?.focus();
            if (open) close();
            else onOpenChange(true);
          }}
        >
          <Icon name={open ? 'chevron-up' : 'chevron-down'} size={14} stroke={2} />
        </button>
      </div>

      {open && (
        <Popover
          anchor={boxRef}
          onDismiss={close}
          fit="anchor"
          class="cp-pop cp-cr-versions-menu"
          // Pressing in the list keeps the caret in the field, so typing carries on.
          onMouseDown={(event) => event.preventDefault()}
        >
          {!searching && (
            <Segmented<Channel>
              role="tablist"
              size="sm"
              full
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
          )}
          {rows.length > 0 ? (
            <ul class="cp-cr-vlist" role="listbox" id={LIST_ID} aria-label="Minecraft versions" ref={listRef}>
              {rows.map((row) => {
                const chosen = selected?.selectionId === row.selectionId;
                const inHand = hand === row;
                const candidate = versions.compatibility(row);
                return (
                  <li
                    key={row.selectionId}
                    id={rowId(row)}
                    class="cp-cr-vrow"
                    role="option"
                    aria-selected={chosen}
                    aria-disabled={!row.createEnabled}
                    data-hand={inHand ? '' : undefined}
                    title={row.disabledReason ?? undefined}
                    onMouseMove={() => {
                      if (!inHand) setReached({ listing, id: row.selectionId });
                    }}
                    onClick={() => accept(row, false)}
                  >
                    {versions.acrossLoaders && (
                      <LoaderLogo
                        loader={loaderKeyFromSourceId(row.sourceId)}
                        size={14}
                        class="cp-cr-loader-mark cp-cr-vrow-loader"
                      />
                    )}
                    <span class="cp-cr-version">{row.displayName}</span>
                    {row.hint && <span class="cp-cr-vrow-hint">{row.hint}</span>}
                    {searching && row.channel !== 'release' && <Chip>{CHANNEL_LABEL[row.channel]}</Chip>}
                    {draft && candidate && <CompatibilityPreview candidate={candidate} draft={draft} />}
                    <span class="cp-head-fill" />
                    {row.downloadState !== 'none' && (
                      <Chip tone="bare" title={installedTitle(row)}>
                        {INSTALLED_LABEL[row.downloadState]}
                      </Chip>
                    )}
                    {searching ? (
                      // Every row keeps the action's place, so nothing moves as the hand does.
                      <span class="cp-cr-vrow-act">
                        {row.createEnabled && (
                          <Button
                            size="sm"
                            sound="affirm"
                            onClick={(event) => {
                              event.stopPropagation();
                              accept(row, true);
                            }}
                          >
                            Create
                          </Button>
                        )}
                      </span>
                    ) : (
                      <span class="cp-cr-vrow-mark" aria-hidden="true">
                        {chosen && <Icon name="check" size={13} stroke={2.6} />}
                      </span>
                    )}
                  </li>
                );
              })}
            </ul>
          ) : (
            <p class="cp-cr-versions-none">
              {loading || versions.searchPending
                ? 'Loading versions…'
                : searching
                  ? 'Nothing matches.'
                  : 'Nothing here.'}
            </p>
          )}
        </Popover>
      )}
    </div>
  );
}
