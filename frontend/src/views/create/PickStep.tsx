import type { JSX } from 'preact';
import { Button, Input } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import type { NewInstanceLoaderMachine } from '../../machines/new-instance-loader';
import type { LoaderBuildRecord } from '../../types';
import {
  LOADER_COMPONENT_IDS,
  LOADER_KEYS,
  LOADER_LABELS,
  LOADER_TAGLINES,
  type Channel,
  type LoaderKey,
} from './defaults';
import { LoaderLogo } from './loader-logos';
import { CHANNEL_LABEL, type VersionRowModel } from './view-model';

export function PickStep({
  source,
  onSourcePick,
  onSourcePreview,
  onSourcePreviewCancel,
  channel,
  channels,
  onChannelChange,
  query,
  onQueryChange,
  searchRef,
  versionListRef,
  rows,
  selectedId,
  onSelectId,
  loaderLoading,
  loaderError,
  loaderMachine,
  selectedBuild,
  catalogLoading,
  catalogError,
  onRetryCatalog,
}: {
  source: LoaderKey;
  onSourcePick: (key: LoaderKey) => void;
  onSourcePreview: (key: LoaderKey) => void;
  onSourcePreviewCancel: () => void;
  channel: Channel;
  channels: Channel[];
  onChannelChange: (channel: Channel) => void;
  query: string;
  onQueryChange: (value: string) => void;
  searchRef: { current: HTMLInputElement | null };
  versionListRef: { current: HTMLDivElement | null };
  rows: VersionRowModel[];
  selectedId: string | null;
  onSelectId: (id: string) => void;
  loaderLoading: boolean;
  loaderError: string | null;
  loaderMachine: NewInstanceLoaderMachine;
  selectedBuild: LoaderBuildRecord | null;
  catalogLoading: boolean;
  catalogError: string | null;
  onRetryCatalog: () => void;
}): JSX.Element {
  const renderSourceGlyph = (key: LoaderKey): JSX.Element => {
    if (key === 'vanilla') return <Icon name="cube" size={15} stroke={1.8} />;
    return <LoaderLogo loader={key} size={15} class="cp-cr-loader-mark" />;
  };

  const subtitle = source === 'vanilla'
    ? 'Pure Minecraft. Pick a version to begin.'
    : `${LOADER_LABELS[source]}. Pick the Minecraft version it should target.`;

  return (
    <section class="cp-cr-step cp-cr-step--pick">
      <header class="cp-cr-head">
        <h1 class="cp-cr-headline">A new world.</h1>
        <p class="cp-cr-subline">{subtitle}</p>
      </header>

      <aside class="cp-cr-rail" role="radiogroup" aria-label="Instance source" aria-orientation="horizontal">
        {LOADER_KEYS.map((key) => (
          <button
            key={key}
            type="button"
            class="cp-cr-rail-item"
            data-active={source === key}
            data-label={LOADER_LABELS[key]}
            data-tag={LOADER_TAGLINES[key]}
            role="radio"
            aria-checked={source === key}
            aria-label={`${LOADER_LABELS[key]}: ${LOADER_TAGLINES[key]}`}
            onClick={() => onSourcePick(key)}
            onPointerEnter={() => onSourcePreview(key)}
            onPointerLeave={onSourcePreviewCancel}
            onFocus={() => onSourcePreview(key)}
            onBlur={onSourcePreviewCancel}
          >
            <span class="cp-cr-rail-glyph">
              {renderSourceGlyph(key)}
            </span>
            <span class="cp-cr-rail-label">{LOADER_LABELS[key]}</span>
          </button>
        ))}
      </aside>

      <div class="cp-cr-vpane">
        <div class="cp-cr-vbar">
          <div class="cp-cr-search">
            <Input
              value={query}
              onChange={onQueryChange}
              placeholder="Filter"
              icon="search"
              inputRef={searchRef}
            />
          </div>
          {channels.length > 1 && (
            <div class="cp-cr-channels" role="tablist" aria-label="Release channel">
              {channels.map((value) => (
                <button
                  key={value}
                  type="button"
                  class="cp-cr-chan"
                  data-active={channel === value}
                  role="tab"
                  aria-selected={channel === value}
                  onClick={() => onChannelChange(value)}
                >
                  {CHANNEL_LABEL[value]}
                </button>
              ))}
            </div>
          )}
        </div>

        <div class="cp-cr-vbody" ref={versionListRef}>
          {catalogLoading && (
            <div class="cp-cr-state">
              <span class="cp-cr-spinner" aria-hidden="true" />
              <span>Loading versions…</span>
            </div>
          )}
          {!catalogLoading && catalogError && (
            <div class="cp-cr-state is-error" role="alert" aria-live="polite">
              <span>Couldn't load the catalog: {catalogError}</span>
              <Button variant="ghost" onClick={onRetryCatalog}>Retry</Button>
            </div>
          )}
          {!catalogLoading && !catalogError && loaderLoading && (
            <div class="cp-cr-state">
              <span class="cp-cr-spinner" aria-hidden="true" />
              <span>Fetching {LOADER_LABELS[source]}…</span>
            </div>
          )}
          {!catalogLoading && !catalogError && loaderError && (
            <div class="cp-cr-state is-error" role="alert" aria-live="polite">
              <span>{loaderError}</span>
              <Button
                variant="ghost"
                onClick={() => {
                  if (source === 'vanilla') return;
                  void loaderMachine.changeComponent(LOADER_COMPONENT_IDS[source], selectedId);
                }}
              >
                Retry
              </Button>
            </div>
          )}

          {!catalogLoading && !catalogError && !loaderLoading && !loaderError && rows.length === 0 && (
            <div class="cp-cr-state is-empty">
              <span>Nothing matches.</span>
            </div>
          )}

          {rows.length > 0 && (
            <ul class="cp-cr-vlist" role="listbox" aria-label="Minecraft versions">
              {rows.map((row, index) => (
                <li
                  key={row.id}
                  class="cp-cr-vrow"
                  data-active={selectedId === row.id}
                  role="option"
                  aria-selected={selectedId === row.id}
                  style={{ ['--i' as any]: String(Math.min(index, 12)) }}
                  onClick={() => onSelectId(row.id)}
                >
                  <span class="cp-cr-vrow-name">{row.displayName}</span>
                  {row.hint && <span class="cp-cr-vrow-hint">{row.hint}</span>}
                  <span class="cp-cr-vrow-spacer" />
                  {row.installed && (
                    <span class="cp-cr-vrow-dot" title="Already installed" aria-hidden="true" />
                  )}
                  <span class="cp-cr-vrow-mark" aria-hidden="true">
                    <Icon name="chevron-right" size={14} stroke={2} />
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>

        {source !== 'vanilla' && selectedId && (
          <div class="cp-cr-build" aria-live="polite">
            {selectedBuild && (
              <span class="cp-cr-build-line">
                <span class="cp-cr-build-key">Build</span>
                <span class="cp-cr-build-value">{selectedBuild.loader_version}</span>
              </span>
            )}
          </div>
        )}
      </div>
    </section>
  );
}
