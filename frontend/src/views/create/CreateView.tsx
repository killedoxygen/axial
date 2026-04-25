import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { catalog, config, versions } from '../../store';
import { setCatalog } from '../../actions';
import { navigate } from '../../ui-state';
import { api } from '../../api';
import { errMessage } from '../../utils';
import {
  createNewInstanceLoaderMachine,
} from '../../machines/new-instance-loader';
import { pickPreferredBuild } from '../../loaders/view-model';
import {
  getCachedLoaderBuilds,
  getCachedLoaderSupportedVersions,
} from '../../loaders/api';
import { createInstance } from '../../instance-create';
import type {
  Catalog, CatalogVersion, LoaderBuildRecord, LoaderComponentId,
} from '../../types';
import {
  channelOfVersion, defaultIconFor, defaultNameFor,
  LOADER_COMPONENT_IDS,
  type Channel, type LoaderKey,
} from './defaults';
import { IdentityStage } from './IdentityStage';
import { SetupStage } from './SetupStage';
import { LibraryBlocker, Stepper, STAGE_ORDER, type Stage } from './shared';
import {
  buildRowModel,
  CHANNEL_ORDER,
  releaseAnchorsFor,
  type VersionRowModel,
} from './view-model';
import { useLoaderHoverPrefetch } from './use-loader-hover-prefetch';
import './create.css';

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

  const loaderMachine = useMemo(() => createNewInstanceLoaderMachine(), []);
  const loaderState = loaderMachine.state.value;
  const currentComponentId = source === 'vanilla' ? null : LOADER_COMPONENT_IDS[source];

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

  const currentSupportedVersions = useMemo(() => {
    if (!currentComponentId) return null;
    if (loaderState.context.selectedComponentId === currentComponentId && loaderState.context.supportedVersions) {
      return loaderState.context.supportedVersions;
    }
    return getCachedLoaderSupportedVersions(currentComponentId);
  }, [currentComponentId, loaderState]);

  const supportedSet = useMemo(() => {
    if (source === 'vanilla') return null;
    if (!currentSupportedVersions) return null;
    return new Set(currentSupportedVersions.map((version) => version.id));
  }, [source, currentSupportedVersions]);

  const availableForSource: CatalogVersion[] = useMemo(() => {
    const cat = catalog.value;
    if (!cat) return [];
    if (source !== 'vanilla' && supportedSet == null) return [];
    return cat.versions.filter((v) => supportedSet == null || supportedSet.has(v.id));
  }, [catalog.value, source, supportedSet]);

  const releaseAnchors = useMemo(() => {
    return releaseAnchorsFor(availableForSource);
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
    if (!currentComponentId || !mcVersionId) return null;
    const builds = loaderState.context.selectedComponentId === currentComponentId
      && loaderState.context.selectedMcVersion === mcVersionId
      ? loaderState.context.builds
      : getCachedLoaderBuilds(currentComponentId, mcVersionId);
    const buildId = loaderState.context.selectedBuildId;
    if (loaderState.context.selectedComponentId === currentComponentId
      && loaderState.context.selectedMcVersion === mcVersionId
      && builds
      && buildId) {
      return builds.find((build) => build.build_id === buildId) ?? null;
    }
    return pickPreferredBuild(builds ?? []);
  }, [currentComponentId, mcVersionId, loaderState]);

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
  const { scheduleHoverPrefetch, cancelHoverPrefetch } = useLoaderHoverPrefetch({
    source,
    mcVersionId,
    latest: catalog.value?.latest,
    loaderMachine,
  });

  useEffect(() => {
    if (!mcVersionId) return;
    if (versionRows.some((r) => r.id === mcVersionId)) return;
    setMcVersionId(null);
  }, [versionRows, mcVersionId]);

  useEffect(() => {
    if (source === 'vanilla' || !mcVersionId) return;
    if (!currentSupportedVersions || !currentComponentId) return;
    void loaderMachine.changeMcVersion(mcVersionId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mcVersionId, source, currentComponentId, currentSupportedVersions]);

  const loaderLoading = source !== 'vanilla'
    && currentSupportedVersions == null
    && (
      loaderState.kind === 'loading_components'
      || loaderState.kind === 'loading_versions'
    );

  const loaderError = source !== 'vanilla'
    && currentSupportedVersions == null
    && loaderState.kind === 'error'
    ? loaderState.context.errorMessage
    : null;

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
                loaderLoading={loaderLoading}
                loaderError={loaderError}
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
