import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, IconButton, Input, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { Slider } from '../../ui/Slider';
import {
  InstanceArt,
  artPresetForSeed,
  loaderTraitForComponentId,
  nextArtSeed,
  versionIdentityForVersion,
  versionIdentityForVersionId,
} from '../../art/InstanceArt';
import { catalog, config, systemInfo, versions } from '../../store';
import { setCatalog } from '../../actions';
import { closeCreate, createOpen } from '../../ui-state';
import { api } from '../../api';
import { errMessage, fmtMem, getMemoryRecommendation } from '../../utils';
import { hashStr } from '../../tokens';
import { Sound } from '../../sound';
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
  LOADER_COMPONENT_IDS, LOADER_KEYS, LOADER_LABELS, LOADER_TAGLINES,
  type Channel, type LoaderKey,
} from './defaults';
import { LoaderLogo } from './loader-logos';
import { LibraryBlocker } from './shared';
import { Modal, ModalContent } from '../../ui/Modal';
import {
  buildRowModel,
  CHANNEL_ORDER,
  CHANNEL_LABEL,
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
import {
  JVM_PRESET_CREATE_ORDER,
  JVM_PRESET_HINTS,
  JVM_PRESET_LABELS,
  type JvmPreset,
} from './jvm-presets';

const BASE_CHANNEL_TABS: Channel[] = ['release', 'snapshot', 'legacy'];

export function CreateView(): JSX.Element {
  const libraryDir = config.value?.library_dir ?? '';
  return (
    <Modal open={createOpen.value} onOpenChange={(next: boolean) => { if (!next) closeCreate(); }}>
      <ModalContent
        className="cp-cr-card"
        aria-label="Create instance"
        aria-describedby={undefined}
        showCloseButton={false}
      >
        {libraryDir ? <CreateCard /> : <LibraryBlocker />}
      </ModalContent>
    </Modal>
  );
}

function CreateCard(): JSX.Element {
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
  useEffect(() => {
    if (!windowPresets.some((p) => p.id === windowPresetId)) {
      setWindowPresetId('default');
    }
  }, [windowPresets, windowPresetId]);

  const cycleWindowPreset = (): void => {
    setWindowPresetId(nextWindowPreset(windowPresets, windowPresetId).id);
  };
  const cycleJvmPreset = (): void => {
    const i = JVM_PRESET_CREATE_ORDER.indexOf(jvmPreset);
    setJvmPreset(JVM_PRESET_CREATE_ORDER[(i + 1) % JVM_PRESET_CREATE_ORDER.length]!);
  };

  const searchInputRef = useRef<HTMLInputElement | null>(null);

  const loaderMachine = useMemo(() => createNewInstanceLoaderMachine(), []);
  const loaderState = loaderMachine.state.value;
  const currentComponentId = source === 'vanilla' ? null : LOADER_COMPONENT_IDS[source];

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
  const displayName = name.trim() || suggestedName || 'New instance';

  const previewSeed = useMemo(() => {
    if (seedOverride != null) return seedOverride;
    const previewId = `preview:${source}:${mcVersionId ?? 'none'}`;
    return hashStr(`${previewId}:${displayName}:${mcVersionId ?? 'preview'}`) || 1;
  }, [seedOverride, source, mcVersionId, displayName]);

  const previewPreset = artPresetForSeed(previewSeed);
  const loaderTrait = source === 'vanilla' ? null : loaderTraitForComponentId(LOADER_COMPONENT_IDS[source]);
  const versionIdentity = (() => {
    const fromVersion = versionIdentityForVersion(selectedMinecraftVersion);
    if (fromVersion) return { ...fromVersion, loaderTrait };
    return versionIdentityForVersionId(mcVersionId ?? '', loaderTrait);
  })();
  const previewInstance = {
    id: `preview:${source}:${mcVersionId ?? 'none'}`,
    name: displayName,
    version_id: mcVersionId ?? '',
    art_seed: previewSeed,
  };

  const rerollSeed = (): void => {
    Sound.ui('click');
    setSeedOverride(nextArtSeed(previewSeed));
  };

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

  const versionReady = Boolean(mcVersionId && (source === 'vanilla' || selectedBuild));
  const canCreate = versionReady && name.trim().length > 0 && !submitting;

  const submit = async (): Promise<void> => {
    if (submitting || !canCreate) return;
    const trimmed = name.trim();
    if (!trimmed || !effectiveVersionId) return;
    setSubmitting(true);
    try {
      const accentLabel = config.value?.theme ?? '';
      const winSpec = windowPresets.find((p) => p.id === windowPresetId);
      const dims = winSpec && winSpec.id !== 'default'
        ? { w: winSpec.w, h: winSpec.h }
        : null;
      const result = await createInstance({
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
      if (result.ok) closeCreate();
    } finally {
      setSubmitting(false);
    }
  };

  useEffect(() => {
    const handler = (e: KeyboardEvent): void => {
      if (submitting) return;
      const target = e.target as HTMLElement | null;
      const inField = target != null
        && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA');

      if (e.key === 'Enter' && (e.ctrlKey || (!inField && target?.tagName !== 'BUTTON'))) {
        if (canCreate) {
          e.preventDefault();
          void submit();
        }
        return;
      }
      if (e.key === '/' && !inField) {
        e.preventDefault();
        searchInputRef.current?.focus();
      }
    };
    window.addEventListener('keydown', handler);
    return () => { window.removeEventListener('keydown', handler); };
  }, [canCreate, submitting, source, mcVersionId, selectedBuild, name, memoryGB, previewSeed, windowPresets, windowPresetId, jvmPreset]);

  const availableChannelSet = new Set(availableChannels);
  const channelTabs: Channel[] = [
    ...BASE_CHANNEL_TABS,
    ...availableChannels.filter((c) => !BASE_CHANNEL_TABS.includes(c)),
  ];

  const winSpec = windowPresets.find((p) => p.id === windowPresetId)
    ?? windowPresets[windowPresets.length - 1]
    ?? { id: 'default', label: 'Default', w: 0, h: 0 };
  const winSubtitle = winSpec.id === 'default' ? 'Game default' : `${winSpec.w} × ${winSpec.h}`;

  return (
    <>
        <header class="cp-cr-card-head">
          <div>
            <h1>Create instance</h1>
            <p>Pick a version, name it, play.</p>
          </div>
          <IconButton icon="x" tooltip="Close (Esc)" onClick={closeCreate} />
        </header>

        <div class="cp-cr-card-body">
          <section class="cp-cr-pick" aria-label="Version">
            <div class="cp-cr-sources" role="radiogroup" aria-label="Instance source">
              {LOADER_KEYS.map((key) => (
                <button
                  key={key}
                  type="button"
                  class="cp-cr-source"
                  data-active={source === key}
                  role="radio"
                  aria-checked={source === key}
                  title={LOADER_TAGLINES[key]}
                  onClick={() => { setSource(key); setMcVersionId(null); }}
                  onPointerEnter={() => scheduleHoverPrefetch(key)}
                  onPointerLeave={cancelHoverPrefetch}
                  onFocus={() => scheduleHoverPrefetch(key)}
                  onBlur={cancelHoverPrefetch}
                >
                  {key === 'vanilla'
                    ? <Icon name="cube" size={14} stroke={1.8} />
                    : <LoaderLogo loader={key} size={14} class="cp-cr-loader-mark" />}
                  <span>{LOADER_LABELS[key]}</span>
                </button>
              ))}
            </div>

            <div class="cp-cr-vbar">
              <Input
                value={query}
                onChange={setQuery}
                placeholder="Filter versions…"
                icon="search"
                inputRef={searchInputRef}
                style={{ flex: 1 }}
              />
              <div class="cp-cr-channels" role="tablist" aria-label="Release channel">
                {channelTabs.map((value) => {
                  const available = availableChannelSet.has(value);
                  return (
                    <button
                      key={value}
                      type="button"
                      class="cp-cr-chan"
                      data-active={channel === value}
                      role="tab"
                      aria-selected={channel === value}
                      disabled={!available}
                      title={available ? undefined : `No ${CHANNEL_LABEL[value].toLowerCase()} versions here`}
                      onClick={() => { if (available) setChannel(value); }}
                    >
                      {CHANNEL_LABEL[value]}
                    </button>
                  );
                })}
              </div>
            </div>

            <div class="cp-cr-vwell">
              {catalogLoading && (
                <div class="cp-cr-state">
                  <span class="cp-cr-spinner" aria-hidden="true" />
                  <span>Loading versions…</span>
                </div>
              )}
              {!catalogLoading && catalogError && (
                <div class="cp-cr-state is-error" role="alert" aria-live="polite">
                  <span>Couldn't load the catalog: {catalogError}</span>
                  <Button variant="ghost" size="sm" onClick={() => { void loadCatalog(); }}>Retry</Button>
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
                    size="sm"
                    onClick={() => {
                      if (source === 'vanilla') return;
                      void loaderMachine.changeComponent(LOADER_COMPONENT_IDS[source], mcVersionId);
                    }}
                  >
                    Retry
                  </Button>
                </div>
              )}
              {!catalogLoading && !catalogError && !loaderLoading && !loaderError && versionRows.length === 0 && (
                <div class="cp-cr-state is-empty">
                  <span>Nothing matches.</span>
                </div>
              )}
              {versionRows.length > 0 && (
                <ul class="cp-cr-vlist" role="listbox" aria-label="Minecraft versions">
                  {versionRows.map((row) => (
                    <li
                      key={row.id}
                      class="cp-cr-vrow"
                      data-active={mcVersionId === row.id}
                      role="option"
                      aria-selected={mcVersionId === row.id}
                      onClick={() => setMcVersionId(row.id)}
                    >
                      <span class="cp-cr-vrow-name">{row.displayName}</span>
                      {row.hint && <span class="cp-cr-vrow-hint">{row.hint}</span>}
                      <span class="cp-cr-vrow-spacer" />
                      {row.installed && (
                        <span class="cp-cr-vrow-installed" title="Already installed">
                          <Icon name="download" size={13} stroke={2} />
                        </span>
                      )}
                      {mcVersionId === row.id && (
                        <span class="cp-cr-vrow-mark" aria-hidden="true">
                          <Icon name="check" size={14} stroke={2.4} />
                        </span>
                      )}
                    </li>
                  ))}
                </ul>
              )}
            </div>

            <div class="cp-cr-pickfoot" aria-live="polite">
              {source !== 'vanilla' && mcVersionId && selectedBuild
                ? <span>{LOADER_LABELS[source]} build <b>{selectedBuild.loader_version}</b></span>
                : source !== 'vanilla' && mcVersionId
                  ? <span>Resolving {LOADER_LABELS[source]} build…</span>
                  : <span>{versionRows.length} version{versionRows.length === 1 ? '' : 's'}</span>}
            </div>
          </section>

          <section class="cp-cr-side" aria-label="Instance details">
            <div class="cp-cr-identity">
              <div class="cp-cr-avatar">
                <InstanceArt
                  instance={previewInstance}
                  versionIdentity={versionIdentity}
                  aspect="square"
                  radius={16}
                />
                <button
                  type="button"
                  class="cp-cr-reroll"
                  title={`Reroll artwork (${previewPreset})`}
                  aria-label="Reroll artwork"
                  onClick={rerollSeed}
                >
                  <Icon name="refresh" size={13} stroke={2} />
                </button>
              </div>
              <div class="cp-cr-identity-text">
                <h2 title={displayName}>{displayName}</h2>
                <div class="cp-cr-identity-pills">
                  <Pill>{source === 'vanilla' ? 'Vanilla' : LOADER_LABELS[source]}</Pill>
                  {mcVersionId
                    ? <Pill>MC {mcVersionId}</Pill>
                    : <Pill>No version yet</Pill>}
                  {versionReady && (
                    <Pill tone={effectiveAlreadyInstalled ? 'ok' : 'neutral'}>
                      {effectiveAlreadyInstalled ? 'Installed' : 'Downloads on create'}
                    </Pill>
                  )}
                </div>
              </div>
            </div>

            <label class="cp-cr-field">
              <span class="cp-cr-field-label">Name</span>
              <Input
                value={name}
                onChange={(v) => setNameOverride(v)}
                placeholder={suggestedName || 'New instance'}
              />
            </label>

            <div class="cp-cr-field">
              <div class="cp-cr-mem-head">
                <span class="cp-cr-field-label">Memory</span>
                <span class="cp-cr-mem-reading" aria-live="polite">{fmtMem(memoryGB)}</span>
              </div>
              <Slider
                value={memoryGB}
                min={1}
                max={totalGB}
                step={0.5}
                recommended={[Math.max(2, memoryRec.rec - 2), Math.min(totalGB, memoryRec.rec + 2)]}
                sound="memory"
                onChange={setMemoryGB}
                ariaLabel="Max memory in gigabytes"
              />
              <span class="cp-cr-hint">
                {memoryGB < 2
                  ? 'Low. May stutter.'
                  : memoryGB > totalGB * 0.75
                    ? 'High. Leave room for the OS.'
                    : `Comfortable start: ${memoryRec.rec} GB.`}
              </span>
            </div>

            <div class="cp-cr-rows" role="group" aria-label="Instance defaults">
              <button type="button" class="cp-cr-row" onClick={cycleWindowPreset} aria-label="Cycle window size">
                <span class="cp-cr-row-key">Window</span>
                <span class="cp-cr-row-val">
                  <span class="cp-cr-row-value">{winSpec.label}</span>
                  <span class="cp-cr-row-sub">{winSubtitle}</span>
                  <Icon name="chevron-right" size={13} stroke={2} />
                </span>
              </button>
              <button
                type="button"
                class="cp-cr-row"
                onClick={cycleJvmPreset}
                aria-label="Cycle performance profile"
                title={JVM_PRESET_HINTS[jvmPreset]}
              >
                <span class="cp-cr-row-key">Profile</span>
                <span class="cp-cr-row-val">
                  <span class="cp-cr-row-value">{JVM_PRESET_LABELS[jvmPreset]}</span>
                  <Icon name="chevron-right" size={13} stroke={2} />
                </span>
              </button>
            </div>
          </section>
        </div>

        <footer class="cp-cr-card-foot">
          <span class="cp-cr-footnote" aria-live="polite">
            {!versionReady
              ? 'Pick a Minecraft version to continue.'
              : canCreate
                ? 'Enter creates the instance.'
                : 'Name your instance to create it.'}
          </span>
          <div class="cp-cr-foot-actions">
            <Button variant="ghost" onClick={closeCreate} disabled={submitting}>
              Cancel
            </Button>
            <Button
              icon="plus"
              onClick={() => { void submit(); }}
              disabled={!canCreate}
              sound="affirm"
            >
              {submitting ? 'Creating…' : 'Create instance'}
            </Button>
          </div>
        </footer>
    </>
  );
}
