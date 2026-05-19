import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { catalog, config, systemInfo, versions } from '../../store';
import { setCatalog } from '../../actions';
import { navigate } from '../../ui-state';
import { api } from '../../api';
import { errMessage, getMemoryRecommendation } from '../../utils';
import { hashStr } from '../../tokens';
import { nextArtSeed } from '../../art/InstanceArt';
import { createNewInstanceLoaderMachine } from '../../machines/new-instance-loader';
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
import { PickStep } from './PickStep';
import { NameStep } from './NameStep';
import { LibraryBlocker, Stepper, STEP_ORDER, type Step } from './shared';
import {
  buildRowModel,
  CHANNEL_ORDER,
  releaseAnchorsFor,
  type VersionRowModel,
} from './view-model';
import { useLoaderHoverPrefetch } from './use-loader-hover-prefetch';
import {
  buildWindowPresets,
  detectMaxScreenSize,
  nextWindowPreset,
  type ScreenSize,
  type WindowPresetSpec,
} from './screen-presets';
import { JVM_PRESET_ORDER, type JvmPreset } from './jvm-presets';
import './create.css';

export function CreateView(): JSX.Element {
  const libraryDir = config.value?.library_dir ?? '';
  if (!libraryDir) {
    return <div class="cp-cr-root"><LibraryBlocker /></div>;
  }
  return <CreateWizard />;
}

function CreateWizard(): JSX.Element {
  const [step, setStep] = useState<Step>('pick');
  const [maxReached, setMaxReached] = useState<number>(0);

  const [source, setSource] = useState<LoaderKey>('vanilla');
  const [mcVersionId, setMcVersionId] = useState<string | null>(null);
  const [channel, setChannel] = useState<Channel>('release');
  const [query, setQuery] = useState('');
  const [nameOverride, setNameOverride] = useState<string | null>(null);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  const totalGB = systemInfo.value?.total_memory_mb
    ? Math.floor(systemInfo.value.total_memory_mb / 1024)
    : 16;
  const memoryRec = getMemoryRecommendation(totalGB);
  const [memoryGB, setMemoryGB] = useState<number>(memoryRec.rec);
  const [seedOverride, setSeedOverride] = useState<number | null>(null);
  const [jvmPreset, setJvmPreset] = useState<JvmPreset>('');

  // Window presets derive from the largest available display. Start with the
  // primary screen so first paint isn't blocked, then upgrade once the Window
  // Management API resolves (if granted).
  const [screenMax, setScreenMax] = useState<ScreenSize>(() => ({
    w: typeof window !== 'undefined' && window.screen ? window.screen.width : 1920,
    h: typeof window !== 'undefined' && window.screen ? window.screen.height : 1080,
  }));
  useEffect(() => {
    let cancelled = false;
    void detectMaxScreenSize().then((s) => { if (!cancelled) setScreenMax(s); });
    return () => { cancelled = true; };
  }, []);
  const windowPresets: WindowPresetSpec[] = useMemo(
    () => buildWindowPresets(screenMax),
    [screenMax],
  );
  const [windowPresetId, setWindowPresetId] = useState<string>('default');
  // If the dynamic preset list no longer contains the current id (rare), fall
  // back to default so the cycle stays meaningful.
  useEffect(() => {
    if (!windowPresets.some((p) => p.id === windowPresetId)) {
      setWindowPresetId('default');
    }
  }, [windowPresets, windowPresetId]);

  const cycleWindowPreset = (): void => {
    setWindowPresetId(nextWindowPreset(windowPresets, windowPresetId).id);
  };
  const cycleJvmPreset = (): void => {
    const i = JVM_PRESET_ORDER.indexOf(jvmPreset);
    setJvmPreset(JVM_PRESET_ORDER[(i + 1) % JVM_PRESET_ORDER.length]!);
  };

  const searchInputRef = useRef<HTMLInputElement | null>(null);
  const versionListRef = useRef<HTMLDivElement | null>(null);
  const nameInputRef = useRef<HTMLInputElement | null>(null);

  const loaderMachine = useMemo(() => createNewInstanceLoaderMachine(), []);
  const loaderState = loaderMachine.state.value;
  const currentComponentId = source === 'vanilla' ? null : LOADER_COMPONENT_IDS[source];

  const idx = STEP_ORDER.indexOf(step);

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
    const has: Record<Channel, boolean> = { release: false, snapshot: false, legacy: false, unknown: false };
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

  const selectedMinecraftVersion = useMemo(() => {
    if (!mcVersionId) return null;
    return availableForSource.find((version) => version.id === mcVersionId) ?? null;
  }, [availableForSource, mcVersionId]);

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

  const previewSeed = useMemo(() => {
    if (seedOverride != null) return seedOverride;
    const previewId = `preview:${source}:${mcVersionId ?? 'none'}`;
    const displayName = name.trim() || suggestedName || 'Untitled';
    return hashStr(`${previewId}:${displayName}:${mcVersionId ?? 'preview'}`) || 1;
  }, [seedOverride, source, mcVersionId, name, suggestedName]);

  const rerollSeed = (): void => { setSeedOverride(nextArtSeed(previewSeed)); };

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

  const stepValid: boolean =
    step === 'pick' ? Boolean(mcVersionId && (source === 'vanilla' || selectedBuild)) :
    step === 'name' ? name.trim().length > 0 && !submitting :
    false;

  const advance = (): void => {
    const next = idx + 1;
    if (next >= STEP_ORDER.length) return;
    setStep(STEP_ORDER[next]!);
    setMaxReached((m) => Math.max(m, next));
  };

  const jumpTo = (i: number): void => {
    const target = STEP_ORDER[i];
    if (!target || i === idx) return;
    if (i > maxReached) return;
    setStep(target);
  };

  const goBack = (): void => { if (idx > 0) jumpTo(idx - 1); };

  const submit = async (): Promise<void> => {
    if (submitting) return;
    const trimmed = name.trim();
    if (!trimmed || !effectiveVersionId) return;
    setSubmitting(true);
    try {
      const accentLabel = config.value?.theme ?? '';
      const winSpec = windowPresets.find((p) => p.id === windowPresetId);
      const dims = winSpec && winSpec.id !== 'default'
        ? { w: winSpec.w, h: winSpec.h }
        : null;
      await createInstance({
        name: trimmed,
        versionId: effectiveVersionId,
        icon: defaultIconFor(source),
        accent: accentLabel,
        install: effectiveAlreadyInstalled
          ? { kind: 'none' }
          : source === 'vanilla'
            ? { kind: 'vanilla', versionId: effectiveVersionId }
            : selectedBuild
              ? { kind: 'loader', build: selectedBuild }
              : { kind: 'none' },
        initialSettings: {
          max_memory_mb: Math.round(memoryGB * 1024),
          art_seed: previewSeed,
          ...(dims ? { window_width: dims.w, window_height: dims.h } : {}),
          ...(jvmPreset ? { jvm_preset: jvmPreset } : {}),
        },
      });
    } finally {
      setSubmitting(false);
    }
  };

  const onPrimary = (): void => {
    if (!stepValid) return;
    if (step === 'name') void submit();
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
        if (stepValid) void submit();
        return;
      }
      if (e.key === 'Enter') {
        if (inField && step !== 'name') return;
        if (target?.tagName === 'BUTTON') return;
        e.preventDefault();
        onPrimary();
        return;
      }
      if (e.key === 'ArrowRight' && !inField) {
        if (stepValid && step !== 'name') { e.preventDefault(); advance(); }
        return;
      }
      if (e.key === 'ArrowLeft' && !inField) {
        if (idx > 0) { e.preventDefault(); goBack(); }
        return;
      }
      if (e.key === '/' && !inField && step === 'pick') {
        e.preventDefault();
        searchInputRef.current?.focus();
      }
    };
    window.addEventListener('keydown', handler);
    return () => { window.removeEventListener('keydown', handler); };
  }, [
    idx,
    step,
    stepValid,
    submitting,
    source,
    mcVersionId,
    selectedBuild,
    name,
    memoryGB,
    previewSeed,
    windowPresets,
    windowPresetId,
    jvmPreset,
  ]);

  return (
    <div class="cp-cr-root">
      <header class="cp-cr-top">
        <Stepper current={idx} maxReached={maxReached} onJump={jumpTo} />
      </header>

      <main class="cp-cr-main">
        <div class="cp-cr-canvas" key={step}>
          {step === 'pick' && (
            <PickStep
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
          {step === 'name' && (
            <NameStep
              source={source}
              mcVersionId={mcVersionId ?? ''}
              name={name}
              suggestedName={suggestedName}
              onNameChange={(v) => setNameOverride(v)}
              nameInputRef={nameInputRef}
              alreadyInstalled={effectiveAlreadyInstalled}
              selectedBuild={selectedBuild}
              minecraftVersion={selectedMinecraftVersion}
              previewSeed={previewSeed}
              onReroll={rerollSeed}
              memoryGB={memoryGB}
              onMemoryChange={setMemoryGB}
              memoryRec={memoryRec.rec}
              totalGB={totalGB}
              windowPresets={windowPresets}
              windowPresetId={windowPresetId}
              onCycleWindow={cycleWindowPreset}
              jvmPreset={jvmPreset}
              onCycleJvm={cycleJvmPreset}
            />
          )}
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
              disabled={!stepValid}
              aria-label={step === 'name' ? 'Create instance' : 'Next step'}
              title={step === 'name' ? 'Create  Ctrl+↵' : 'Next  →'}
            >
              {step === 'name'
                ? <Icon name="check" size={18} stroke={2.2} />
                : <Icon name="chevron-right" size={18} stroke={2.2} />}
            </button>
          </div>
        </div>
        <div class="cp-cr-footnote" aria-live="polite">
          {step === 'pick'
            ? mcVersionId
              ? 'Enter to continue · / to filter · Esc to leave.'
              : 'Pick a version · / to filter.'
            : name.trim()
              ? 'Press Ctrl + Enter to create.'
              : 'Name your world to create.'}
        </div>
      </footer>
    </div>
  );
}
