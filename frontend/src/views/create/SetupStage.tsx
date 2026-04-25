import type { JSX } from 'preact';
import { Button, Input } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import type {
  NewInstanceLoaderMachine,
  NewInstanceLoaderState,
} from '../../machines/new-instance-loader';
import type { LoaderBuildRecord } from '../../types';
import {
  LOADER_COMPONENT_IDS,
  LOADER_KEYS,
  LOADER_LABELS,
  LOADER_TAGLINES,
  type Channel,
  type LoaderKey,
} from './defaults';
import { Words } from './shared';
import { CHANNEL_LABEL, type VersionRowModel } from './view-model';

const SOURCE_ICON: Record<LoaderKey, string> = {
  vanilla: 'cube',
  fabric: 'compass',
  quilt: 'palette',
  forge: 'terminal',
  neoforge: 'rectangle',
};

export function SetupStage({
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
  loaderState,
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
  loaderState: NewInstanceLoaderState;
  loaderMachine: NewInstanceLoaderMachine;
  selectedBuild: LoaderBuildRecord | null;
  catalogLoading: boolean;
  catalogError: string | null;
  onRetryCatalog: () => void;
}): JSX.Element {
  const loaderLoading = source !== 'vanilla' && (
    loaderState.kind === 'loading_components'
    || loaderState.kind === 'loading_versions'
  );
  const loaderError = source !== 'vanilla' && loaderState.kind === 'error'
    ? loaderState.context.errorMessage
    : null;

  return (
    <>
      <header class="cp-cr-head">
        <h1 class="cp-cr-headline"><Words text="A new world." /></h1>
        <p class="cp-cr-subline">
          {source === 'vanilla'
            ? 'Pure Minecraft. Pick a version to start with.'
            : `${LOADER_LABELS[source]}. Pick the Minecraft version it should target.`}
        </p>
      </header>

      <div class="cp-cr-setup">
        <aside class="cp-cr-rail" role="radiogroup" aria-label="Instance source">
          {LOADER_KEYS.map((key, index) => (
            <button
              key={key}
              type="button"
              class="cp-cr-rail-item"
              data-active={source === key}
              role="radio"
              aria-checked={source === key}
              style={{ ['--i' as any]: String(index) }}
              onClick={() => onSourcePick(key)}
              onPointerEnter={() => onSourcePreview(key)}
              onPointerLeave={onSourcePreviewCancel}
              onFocus={() => onSourcePreview(key)}
              onBlur={onSourcePreviewCancel}
            >
              <span class="cp-cr-rail-glyph">
                <Icon name={SOURCE_ICON[key]} size={15} stroke={1.8} />
              </span>
              <span class="cp-cr-rail-label">
                <span class="cp-cr-rail-name">{LOADER_LABELS[key]}</span>
                <span class="cp-cr-rail-tag">{LOADER_TAGLINES[key]}</span>
              </span>
            </button>
          ))}
          <div class="cp-cr-rail-item is-soon" aria-disabled="true" style={{ ['--i' as any]: String(LOADER_KEYS.length) }}>
            <span class="cp-cr-rail-glyph">
              <Icon name="download" size={15} stroke={1.8} />
            </span>
            <span class="cp-cr-rail-label">
              <span class="cp-cr-rail-name">Modpack</span>
              <span class="cp-cr-rail-tag">Modrinth · soon</span>
            </span>
          </div>
        </aside>

        <section class="cp-cr-vpane">
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
        </section>
      </div>
    </>
  );
}
