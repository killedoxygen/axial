import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Input } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { AccentField } from '../settings/AccentEditor';
import { catalog, config, versions } from '../../store';
import { setCatalog } from '../../actions';
import { navigate } from '../../ui-state';
import { api } from '../../api';
import { errMessage, isReleaseVersion, isSnapshotVersion, parseVersionDisplay } from '../../utils';
import {
  createNewInstanceLoaderMachine,
  type NewInstanceLoaderMachine, type NewInstanceLoaderState,
} from '../../machines/new-instance-loader';
import { createInstance } from '../../instance-create';
import type {
  Catalog, CatalogVersion, LoaderBuildRecord, LoaderComponentId,
} from '../../types';
import {
  channelOfVersion, defaultIconFor, defaultNameFor, INSTANCE_ICON_CHOICES,
  LOADER_COMPONENT_IDS, LOADER_KEYS, LOADER_LABELS, LOADER_TAGLINES,
  type Channel, type LoaderKey,
} from './defaults';
import './create.css';

type Stage = 'setup' | 'identity';
const STAGE_ORDER: Stage[] = ['setup', 'identity'];
const STAGE_LABELS: Record<Stage, string> = {
  setup: 'Setup',
  identity: 'Identity',
};

const SOURCE_ICON: Record<LoaderKey, string> = {
  vanilla: 'cube',
  fabric: 'compass',
  quilt: 'palette',
  forge: 'terminal',
  neoforge: 'rectangle',
};

const CHANNEL_LABEL: Record<Channel, string> = {
  release: 'Release',
  snapshot: 'Snapshot',
  legacy: 'Legacy',
};

const CHANNEL_ORDER: Channel[] = ['release', 'snapshot', 'legacy'];
const LOADER_HOVER_PREFETCH_DELAY_MS = 140;
const LOADER_HOVER_IDLE_TIMEOUT_MS = 500;

type IdleCallbackHandle = number;
type IdleCallbackDeadline = {
  didTimeout: boolean;
  timeRemaining: () => number;
};

type IdleCapableWindow = Window & {
  requestIdleCallback?: (
    callback: (deadline: IdleCallbackDeadline) => void,
    options?: { timeout: number },
  ) => IdleCallbackHandle;
  cancelIdleCallback?: (handle: IdleCallbackHandle) => void;
};

function Words({ text }: { text: string }): JSX.Element {
  const parts = text.split(' ');
  return (
    <>
      {parts.flatMap((w, i) => {
        const span = (
          <span key={`w${i}`} class="cp-cr-word" style={{ ['--i' as any]: String(i) }}>
            {w}
          </span>
        );
        return i === 0 ? [span] : [' ', span];
      })}
    </>
  );
}

function Stepper({
  current, maxReached, onJump,
}: {
  current: number;
  maxReached: number;
  onJump: (i: number) => void;
}): JSX.Element {
  const nodes: JSX.Element[] = [];
  STAGE_ORDER.forEach((s, i) => {
    if (i > 0) {
      nodes.push(<span key={`sep-${i}`} class="cp-cr-stepper-sep" aria-hidden="true">/</span>);
    }
    const state = i < current ? 'past' : i === current ? 'active' : 'future';
    const clickable = i !== current && i <= maxReached;
    const label = STAGE_LABELS[s];
    const num = String(i + 1).padStart(2, '0');
    const inner = (
      <>
        <span class="cp-cr-stepper-num">{num}</span>
        <span class="cp-cr-stepper-label">{label}</span>
      </>
    );
    if (clickable) {
      nodes.push(
        <button
          key={s}
          type="button"
          class="cp-cr-stepper-item"
          data-state={state}
          onClick={() => onJump(i)}
          aria-label={`Go to ${label}`}
        >
          {inner}
        </button>,
      );
    } else {
      nodes.push(
        <div
          key={s}
          class="cp-cr-stepper-item"
          data-state={state}
          aria-current={state === 'active' ? 'step' : undefined}
        >
          {inner}
        </div>,
      );
    }
  });
  return <nav class="cp-cr-stepper" aria-label="Create instance progress">{nodes}</nav>;
}

interface VersionRowModel {
  id: string;
  displayName: string;
  hint: string | null;
  channel: Channel;
  installed: boolean;
}

function LibraryBlocker(): JSX.Element {
  return (
    <div class="cp-cr-blocker">
      <Icon name="folder" size={32} />
      <h2>Set up your library first</h2>
      <p>Croopor needs a place to keep game files before you can make an instance.</p>
      <Button icon="settings" onClick={() => navigate({ name: 'settings' })}>
        Open setup
      </Button>
    </div>
  );
}

export function CreateView(): JSX.Element {
  const libraryDir = config.value?.library_dir ?? '';
  if (!libraryDir) {
    return <div class="cp-cr-root"><LibraryBlocker /></div>;
  }
  return <CreateWizard />;
}

function CreateWizard(): JSX.Element {
  const [stage, setStage] = useState<Stage>('setup');
  const [maxReached, setMaxReached] = useState<number>(0);

  const [source, setSource] = useState<LoaderKey>('vanilla');
  const [mcVersionId, setMcVersionId] = useState<string | null>(null);
  const [channel, setChannel] = useState<Channel>('release');
  const [query, setQuery] = useState('');
  const [nameOverride, setNameOverride] = useState<string | null>(null);
  const [icon, setIcon] = useState<string>(defaultIconFor('vanilla'));
  const [iconOverride, setIconOverride] = useState(false);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  const searchInputRef = useRef<HTMLInputElement | null>(null);
  const versionListRef = useRef<HTMLDivElement | null>(null);
  const hoverPrefetchTimeoutRef = useRef<number | null>(null);
  const hoverPrefetchIdleRef = useRef<IdleCallbackHandle | null>(null);
  const prefetchedComponentsRef = useRef<Set<LoaderComponentId>>(new Set());
  const prefetchingComponentsRef = useRef<Set<LoaderComponentId>>(new Set());

  const loaderMachine = useMemo(() => createNewInstanceLoaderMachine(), []);
  const loaderState = loaderMachine.state.value;

  const idx = STAGE_ORDER.indexOf(stage);

  const loadCatalog = async (): Promise<void> => {
    setCatalogLoading(true);
    setCatalogError(null);
    try {
      const res = (await api('GET', '/catalog')) as (Catalog & { error?: string });
      if (res.error) throw new Error(res.error);
      setCatalog({ latest: res.latest, versions: res.versions });
    } catch (err: unknown) {
      setCatalogError(errMessage(err));
    } finally {
      setCatalogLoading(false);
    }
  };

  useEffect(() => {
    if (catalog.value || catalogLoading) return;
    void loadCatalog();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (source === 'vanilla') {
      loaderMachine.disable();
      return;
    }
    const componentId: LoaderComponentId = LOADER_COMPONENT_IDS[source];
    void loaderMachine.changeComponent(componentId, mcVersionId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [source]);

  useEffect(() => () => { loaderMachine.disable(); }, [loaderMachine]);

  useEffect(() => {
    if (iconOverride) return;
    setIcon(defaultIconFor(source));
  }, [source, iconOverride]);

  const supportedSet = useMemo(() => {
    if (source === 'vanilla') return null;
    const supported = loaderState.context.supportedVersions;
    if (!supported) return null;
    return new Set(supported.map((v) => v.id));
  }, [source, loaderState]);

  const availableForSource: CatalogVersion[] = useMemo(() => {
    const cat = catalog.value;
    if (!cat) return [];
    if (source !== 'vanilla' && supportedSet == null) return [];
    return cat.versions.filter((v) => supportedSet == null || supportedSet.has(v.id));
  }, [catalog.value, source, supportedSet]);

  const releaseAnchors = useMemo(() => {
    return availableForSource
      .filter(isReleaseVersion)
      .slice()
      .sort((a, b) => (a.release_time || '').localeCompare(b.release_time || ''));
  }, [availableForSource]);

  const availableChannels = useMemo<Channel[]>(() => {
    const has: Record<Channel, boolean> = { release: false, snapshot: false, legacy: false };
    for (const v of availableForSource) has[channelOfVersion(v)] = true;
    return CHANNEL_ORDER.filter((c) => has[c]);
  }, [availableForSource]);

  useEffect(() => {
    if (availableChannels.length === 0) return;
    if (availableChannels.includes(channel)) return;
    setChannel(availableChannels[0]!);
  }, [availableChannels, channel]);

  const versionRows: VersionRowModel[] = useMemo(() => {
    const cat = catalog.value;
    if (!cat) return [];
    const installedSet = new Set(
      versions.value.filter((x) => x.installed && x.launchable).map((x) => x.id),
    );
    const q = query.trim().toLowerCase();
    const rows = availableForSource
      .filter((v) => channelOfVersion(v) === channel)
      .filter((v) => !q
        || v.id.toLowerCase().includes(q)
        || v.minecraft_meta.display_name.toLowerCase().includes(q));
    rows.sort((a, b) => (b.release_time || '').localeCompare(a.release_time || ''));
    return rows.map((v) => buildRowModel(v, releaseAnchors, installedSet, source));
  }, [catalog.value, versions.value, channel, query, availableForSource, releaseAnchors, source]);

  const selectedBuild: LoaderBuildRecord | null = useMemo(() => {
    if (source === 'vanilla' || !mcVersionId) return null;
    if (loaderState.context.selectedMcVersion !== mcVersionId) return null;
    const builds = loaderState.context.builds;
    const buildId = loaderState.context.selectedBuildId;
    if (!builds || !buildId) return null;
    return builds.find((b) => b.build_id === buildId) ?? null;
  }, [source, mcVersionId, loaderState]);

  const effectiveVersionId: string = useMemo(() => {
    if (source === 'vanilla') return mcVersionId ?? '';
    return selectedBuild?.version_id ?? '';
  }, [source, mcVersionId, selectedBuild]);

  const effectiveAlreadyInstalled: boolean = useMemo(() => {
    if (!effectiveVersionId) return false;
    return versions.value.some((v) => v.id === effectiveVersionId && v.installed && v.launchable);
  }, [effectiveVersionId, versions.value]);

  const suggestedName = useMemo(() => {
    if (!mcVersionId) return '';
    return defaultNameFor(source, mcVersionId);
  }, [source, mcVersionId]);

  const name = nameOverride ?? suggestedName;

  const hoverPrefetchVersions = useMemo(() => {
    const ids = new Set<string>();
    if (mcVersionId) ids.add(mcVersionId);
    const latest = catalog.value?.latest;
    if (latest?.release) ids.add(latest.release);
    if (latest?.snapshot) ids.add(latest.snapshot);
    return Array.from(ids).slice(0, 3);
  }, [mcVersionId, catalog.value]);

  const cancelHoverPrefetch = (): void => {
    if (hoverPrefetchTimeoutRef.current != null) {
      window.clearTimeout(hoverPrefetchTimeoutRef.current);
      hoverPrefetchTimeoutRef.current = null;
    }
    const idleWindow = window as IdleCapableWindow;
    if (hoverPrefetchIdleRef.current != null && idleWindow.cancelIdleCallback) {
      idleWindow.cancelIdleCallback(hoverPrefetchIdleRef.current);
      hoverPrefetchIdleRef.current = null;
    }
  };

  const runHoverPrefetch = (componentId: LoaderComponentId): void => {
    if (prefetchedComponentsRef.current.has(componentId)) return;
    if (prefetchingComponentsRef.current.has(componentId)) return;
    prefetchingComponentsRef.current.add(componentId);
    void loaderMachine.prefetchComponent(componentId, hoverPrefetchVersions)
      .then(() => {
        prefetchedComponentsRef.current.add(componentId);
      })
      .finally(() => {
        prefetchingComponentsRef.current.delete(componentId);
      });
  };

  const scheduleHoverPrefetch = (loaderKey: LoaderKey): void => {
    if (loaderKey === 'vanilla' || loaderKey === source) return;
    const componentId = LOADER_COMPONENT_IDS[loaderKey];
    if (prefetchedComponentsRef.current.has(componentId)) return;
    if (prefetchingComponentsRef.current.has(componentId)) return;
    cancelHoverPrefetch();
    hoverPrefetchTimeoutRef.current = window.setTimeout(() => {
      hoverPrefetchTimeoutRef.current = null;
      const idleWindow = window as IdleCapableWindow;
      if (idleWindow.requestIdleCallback) {
        hoverPrefetchIdleRef.current = idleWindow.requestIdleCallback(() => {
          hoverPrefetchIdleRef.current = null;
          runHoverPrefetch(componentId);
        }, { timeout: LOADER_HOVER_IDLE_TIMEOUT_MS });
        return;
      }
      runHoverPrefetch(componentId);
    }, LOADER_HOVER_PREFETCH_DELAY_MS);
  };

  useEffect(() => {
    if (!mcVersionId) return;
    if (versionRows.some((r) => r.id === mcVersionId)) return;
    setMcVersionId(null);
  }, [versionRows, mcVersionId]);

  useEffect(() => {
    if (source === 'vanilla' || !mcVersionId) return;
    if (!loaderState.context.supportedVersions) return;
    void loaderMachine.changeMcVersion(mcVersionId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mcVersionId, source, loaderState.context.supportedVersions]);

  useEffect(() => () => { cancelHoverPrefetch(); }, []);

  const stageValid: boolean =
    stage === 'setup'    ? Boolean(mcVersionId && (source === 'vanilla' || selectedBuild)) :
    stage === 'identity' ? name.trim().length > 0 && !submitting :
    false;

  const advance = (): void => {
    const next = idx + 1;
    if (next >= STAGE_ORDER.length) return;
    setStage(STAGE_ORDER[next]!);
    setMaxReached((m) => Math.max(m, next));
  };

  const jumpTo = (i: number): void => {
    const target = STAGE_ORDER[i];
    if (!target || i === idx) return;
    if (i > maxReached) return;
    setStage(target);
  };

  const goBack = (): void => { if (idx > 0) jumpTo(idx - 1); };

  const submit = async (): Promise<void> => {
    if (submitting) return;
    const trimmed = name.trim();
    if (!trimmed || !effectiveVersionId) return;
    setSubmitting(true);
    try {
      const accentLabel = config.value?.theme ?? '';
      await createInstance({
        name: trimmed,
        versionId: effectiveVersionId,
        icon,
        accent: accentLabel,
        install: effectiveAlreadyInstalled
          ? { kind: 'none' }
          : source === 'vanilla'
            ? { kind: 'vanilla', versionId: effectiveVersionId }
            : selectedBuild
              ? { kind: 'loader', build: selectedBuild }
              : { kind: 'none' },
      });
    } finally {
      setSubmitting(false);
    }
  };

  const onPrimary = (): void => {
    if (!stageValid) return;
    if (stage === 'identity') void submit();
    else advance();
  };

  useEffect(() => {
    const handler = (e: KeyboardEvent): void => {
      if (submitting) return;
      const target = e.target as HTMLElement | null;
      const inField = target != null
        && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA');

      if (e.key === 'Escape' && !inField) {
        if (idx === 0) { navigate({ name: 'instances' }); return; }
        e.preventDefault();
        goBack();
        return;
      }
      if (e.key === 'Enter' && e.ctrlKey) {
        e.preventDefault();
        if (stageValid) void submit();
        return;
      }
      if (e.key === 'Enter') {
        if (inField && stage !== 'identity') return;
        if (target?.tagName === 'BUTTON') return;
        e.preventDefault();
        onPrimary();
        return;
      }
      if (e.key === 'ArrowRight' && !inField) {
        if (stageValid && stage !== 'identity') { e.preventDefault(); advance(); }
        return;
      }
      if (e.key === 'ArrowLeft' && !inField) {
        if (idx > 0) { e.preventDefault(); goBack(); }
        return;
      }
      if (e.key === '/' && !inField && stage === 'setup') {
        e.preventDefault();
        searchInputRef.current?.focus();
      }
    };
    window.addEventListener('keydown', handler);
    return () => { window.removeEventListener('keydown', handler); };
  }, [idx, stage, stageValid, submitting, source, mcVersionId, selectedBuild, name]);

  return (
    <div class="cp-cr-root">
      <div class="cp-cr-statusbar">
        <Stepper current={idx} maxReached={maxReached} onJump={jumpTo} />
      </div>

      <main class="cp-cr-main">
        <div class={`cp-cr-column cp-cr-column--${stage}`} key={stage}>
          <div class="cp-cr-stage">
            {stage === 'setup' && (
              <SetupStage
                source={source}
                onSourcePick={(k) => { setSource(k); setMcVersionId(null); }}
                onSourcePreview={scheduleHoverPrefetch}
                onSourcePreviewCancel={cancelHoverPrefetch}
                channel={channel}
                channels={availableChannels}
                onChannelChange={setChannel}
                query={query}
                onQueryChange={setQuery}
                searchRef={searchInputRef}
                versionListRef={versionListRef}
                rows={versionRows}
                selectedId={mcVersionId}
                onSelectId={setMcVersionId}
                loaderState={loaderState}
                loaderMachine={loaderMachine}
                selectedBuild={selectedBuild}
                catalogLoading={catalogLoading}
                catalogError={catalogError}
                onRetryCatalog={() => { void loadCatalog(); }}
              />
            )}
            {stage === 'identity' && (
              <IdentityStage
                source={source}
                mcVersionId={mcVersionId ?? ''}
                name={name}
                suggestedName={suggestedName}
                onNameChange={(v) => setNameOverride(v)}
                icon={icon}
                onIconPick={(name) => { setIcon(name); setIconOverride(true); }}
                alreadyInstalled={effectiveAlreadyInstalled}
                selectedBuild={selectedBuild}
              />
            )}
          </div>
        </div>
      </main>

      <footer class="cp-cr-bottom">
        <div class="cp-cr-actions">
          <Button variant="ghost" onClick={() => navigate({ name: 'instances' })} disabled={submitting}>
            Cancel
          </Button>
          <div class="cp-cr-nav" role="group" aria-label="Step navigation">
            <button
              type="button"
              class="cp-cr-navbtn"
              onClick={goBack}
              disabled={idx === 0 || submitting}
              aria-label="Previous step"
              title="Previous  ←"
            >
              <Icon name="chevron-left" size={18} stroke={2.2} />
            </button>
            <button
              type="button"
              class="cp-cr-navbtn"
              onClick={onPrimary}
              disabled={!stageValid}
              aria-label={stage === 'identity' ? 'Create instance' : 'Next step'}
              title={stage === 'identity' ? 'Create  Ctrl+↵' : 'Next  →'}
            >
              {stage === 'identity'
                ? <Icon name="check" size={18} stroke={2.2} />
                : <Icon name="chevron-right" size={18} stroke={2.2} />}
            </button>
          </div>
        </div>
        <div class="cp-cr-footnote">
          {stage === 'identity'
            ? 'Press Ctrl + Enter to create.'
            : 'Enter to continue · Esc to go back · / to search.'}
        </div>
      </footer>
    </div>
  );
}

function buildRowModel(
  v: CatalogVersion,
  releaseAnchors: CatalogVersion[],
  installedSet: Set<string>,
  source: LoaderKey,
): VersionRowModel {
  const display = parseVersionDisplay(v.id, v, releaseAnchors);
  let hint = display.hint;
  if (!hint && isSnapshotVersion(v) && v.release_time) {
    const t = v.release_time;
    let nearest: CatalogVersion | null = null;
    for (const r of releaseAnchors) {
      if ((r.release_time || '') >= t) { nearest = r; break; }
    }
    if (!nearest && releaseAnchors.length > 0) {
      nearest = releaseAnchors[releaseAnchors.length - 1] ?? null;
    }
    if (nearest && !v.id.includes(nearest.id)) hint = `~ ${nearest.id}`;
  }
  return {
    id: v.id,
    displayName: display.name === v.id ? v.id : display.name,
    hint: hint && hint !== display.name ? hint : null,
    channel: channelOfVersion(v),
    installed: source === 'vanilla' && (v.installed || installedSet.has(v.id)),
  };
}

// ── Setup stage (combined source + version) ───────────────────────────

function SetupStage({
  source, onSourcePick,
  onSourcePreview,
  onSourcePreviewCancel,
  channel, channels, onChannelChange,
  query, onQueryChange, searchRef, versionListRef,
  rows, selectedId, onSelectId,
  loaderState, loaderMachine, selectedBuild,
  catalogLoading, catalogError, onRetryCatalog,
}: {
  source: LoaderKey;
  onSourcePick: (k: LoaderKey) => void;
  onSourcePreview: (k: LoaderKey) => void;
  onSourcePreviewCancel: () => void;
  channel: Channel;
  channels: Channel[];
  onChannelChange: (c: Channel) => void;
  query: string;
  onQueryChange: (v: string) => void;
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
          {LOADER_KEYS.map((k, i) => (
            <button
              key={k}
              type="button"
              class="cp-cr-rail-item"
              data-active={source === k}
              role="radio"
              aria-checked={source === k}
              style={{ ['--i' as any]: String(i) }}
              onClick={() => onSourcePick(k)}
              onPointerEnter={() => onSourcePreview(k)}
              onPointerLeave={onSourcePreviewCancel}
              onFocus={() => onSourcePreview(k)}
              onBlur={onSourcePreviewCancel}
            >
              <span class="cp-cr-rail-glyph">
                <Icon name={SOURCE_ICON[k]} size={15} stroke={1.8} />
              </span>
              <span class="cp-cr-rail-label">
                <span class="cp-cr-rail-name">{LOADER_LABELS[k]}</span>
                <span class="cp-cr-rail-tag">{LOADER_TAGLINES[k]}</span>
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
                {channels.map((c) => (
                  <button
                    key={c}
                    type="button"
                    class="cp-cr-chan"
                    data-active={channel === c}
                    role="tab"
                    aria-selected={channel === c}
                    onClick={() => onChannelChange(c)}
                  >
                    {CHANNEL_LABEL[c]}
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
                {rows.map((row, i) => (
                  <li
                    key={row.id}
                    class="cp-cr-vrow"
                    data-active={selectedId === row.id}
                    role="option"
                    aria-selected={selectedId === row.id}
                    style={{ ['--i' as any]: String(Math.min(i, 12)) }}
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

function IdentityStage({
  source, mcVersionId, name, suggestedName, onNameChange, icon, onIconPick,
  alreadyInstalled, selectedBuild,
}: {
  source: LoaderKey;
  mcVersionId: string;
  name: string;
  suggestedName: string;
  onNameChange: (v: string) => void;
  icon: string;
  onIconPick: (name: string) => void;
  alreadyInstalled: boolean;
  selectedBuild: LoaderBuildRecord | null;
}): JSX.Element {
  const summary = source === 'vanilla'
    ? `Vanilla · ${mcVersionId}`
    : selectedBuild
      ? `${LOADER_LABELS[source]} ${selectedBuild.loader_version} · ${mcVersionId}`
      : `${LOADER_LABELS[source]} · ${mcVersionId}`;

  return (
    <>
      <header class="cp-cr-head">
        <h1 class="cp-cr-headline"><Words text="Name it." /></h1>
        <p class="cp-cr-subline">
          {summary}{alreadyInstalled ? '' : ' · downloads after create'}
        </p>
      </header>

      <div class="cp-cr-id">
        <div class="cp-cr-id-card">
          <div class="cp-cr-id-preview" data-icon={icon}>
            <span class="cp-cr-id-preview-glyph">
              <Icon name={icon} size={28} stroke={1.6} />
            </span>
            <span class="cp-cr-id-preview-name">{name.trim() || suggestedName || 'Untitled'}</span>
          </div>

          <div class="cp-cr-id-row">
            <label class="cp-cr-id-label">Name</label>
            <Input
              value={name}
              onChange={(v) => onNameChange(v)}
              placeholder={suggestedName || 'Aurora Adventure'}
              autoFocus
            />
          </div>

          <div class="cp-cr-id-row">
            <label class="cp-cr-id-label">Icon</label>
            <div class="cp-cr-iconrow" role="radiogroup" aria-label="Instance icon">
              {INSTANCE_ICON_CHOICES.map((n, i) => (
                <button
                  key={n}
                  type="button"
                  class="cp-cr-iconbtn"
                  data-active={icon === n}
                  aria-label={n}
                  aria-checked={icon === n}
                  role="radio"
                  style={{ ['--i' as any]: String(i) }}
                  onClick={() => onIconPick(n)}
                >
                  <Icon name={n} size={16} />
                </button>
              ))}
            </div>
          </div>

          <div class="cp-cr-id-row">
            <label class="cp-cr-id-label">Accent</label>
            <AccentField showPresets={true} />
          </div>
        </div>
      </div>
    </>
  );
}
