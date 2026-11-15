import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { nextArtSeed } from '../ui/InstanceVisual';
import { harmoniousTilePalette, seedForTileHue } from '../ui/look-guardian';
import { useTheme } from '../app/use-theme';
import { config, systemInfo } from '../settings/state';
import { closeCreate, createDraft, createModpack, createQuick, type CreateDraftItem } from '../app/route';
import { firstTime, habit, noteHabit } from '../app/habits';
import { loaderBuilds as fetchLoaderBuilds } from './api';
import { contentCompatibility, planInstanceSetup } from '../content/api';
import type { CompatCandidate } from '../generated/CompatCandidate';
import type { ResolutionSelection } from '../generated/ResolutionSelection';
import { recheck, staging } from '../discover/fit';
import { addPicksTo } from '../discover/make';
import { selectionOf, unstageAll } from '../discover/state';
import { errMessage, toast } from '../app/toast';
import { getMemoryRecommendation } from '../ui/MemoryField';
import { hashStr } from '../app/tokens';
import { Sound } from '../app/sound';
import { createInstance } from './create-instance';
import type { CreateNotice } from '../generated/CreateNotice';
import type { CreateLoaderBuildsView } from '../generated/CreateLoaderBuildsView';
import type { CreateOption } from '../generated/CreateOption';
import type { CreateView } from '../generated/CreateView';
import type { CreateVersionRow } from '../generated/CreateVersionRow';
import type { PresetOption } from '../generated/PresetOption';
import { buildWindowPresets, detectMaxScreenSize, type ScreenSize, type WindowPresetSpec } from '../ui/screen-presets';
import { defaultIconFor, defaultNameFor, LOADER_LABELS, type Channel, type LoaderKey } from './defaults';
import { cachedView, fetchView, storeView } from './catalog';
import { useVersionSearch } from './find';
import {
  CHANNEL_ORDER,
  loaderKeyFromSourceId,
  createLoaderSelection,
  normalizeChannel,
  sourceIdFromLoaderKey,
  versionRow,
  type CreateSourceId,
  type VersionRowModel,
} from './versions';

/* A search lists its closest matches, not every version that contains a digit. */
const SEARCH_LIMIT = 40;

function loaderKeyFromWire(value: string | null | undefined): LoaderKey {
  if (!value || value === 'vanilla') return 'vanilla';
  if (value === 'fabric' || value === 'quilt' || value === 'forge' || value === 'neoforge') return value;
  throw new Error('Create loader response was invalid.');
}

/** The create form's one state owner: source, version, identity, options, staged picks and submission. */
export function useCreateForm() {
  const pack = createModpack.value;
  const draft = createDraft.value;
  // Picks alone choose their loader and version; with a pack they follow the pack's.
  const openDraft = pack ? null : draft;
  const theme = useTheme();
  // Opened on a version found elsewhere: the form starts on it and creates it once its defaults are in.
  const [quick] = useState(() => (pack || draft ? null : createQuick.value));
  const [sourceId, setSourceId] = useState<CreateSourceId>(quick?.sourceId ?? 'vanilla');
  const [selectedSelectionId, setSelectedSelectionId] = useState<string | null>(quick?.selectionId ?? null);
  const [channel, setChannel] = useState<Channel>(quick?.channel ?? 'release');
  const [query, setQuery] = useState('');
  const [nameOverride, setNameOverride] = useState<string | null>(null);
  const [viewError, setViewError] = useState<string | null>(null);
  const [viewLoading, setViewLoading] = useState(Boolean(openDraft));
  const [backendView, setBackendView] = useState<CreateView | null>(() => (openDraft ? null : cachedView(sourceId)));
  // The loader whose view's defaults the form now holds.
  const [appliedSource, setAppliedSource] = useState<CreateSourceId | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // The selection Enter asked to create, held until its loader builds arrive.
  const [queuedCreate, setQueuedCreate] = useState<string | null>(quick?.selectionId ?? null);
  const submittingRef = useRef(false);
  const loadRequestRef = useRef(0);
  const loadedCreateViewSourceRef = useRef<CreateSourceId | null>(null);
  const sourceKey = loaderKeyFromSourceId(sourceId);

  const totalGB = systemInfo.value?.total_memory_mb ? Math.floor(systemInfo.value.total_memory_mb / 1024) : 16;
  const memoryRec = getMemoryRecommendation(totalGB);
  const [memoryGB, setMemoryGB] = useState<number>(memoryRec.rec);
  const [paletteSeedOverride, setPaletteSeedOverride] = useState<number | null>(null);
  const [jvmPreset, setJvmPreset] = useState('');
  const [presetOptions, setPresetOptions] = useState<PresetOption[]>([]);
  const [optimizeChoice, setOptimizeChoice] = useState<boolean | null>(null);
  const [loaderChoice, setLoaderChoice] = useState<string | null>(null);
  const [loaderBuilds, setLoaderBuilds] = useState<CreateLoaderBuildsView | null>(null);
  const [loaderBuildsError, setLoaderBuildsError] = useState<string | null>(null);

  const [draftCandidates, setDraftCandidates] = useState<CompatCandidate[] | null>(null);
  const [draftError, setDraftError] = useState<string | null>(null);
  const [draftAutoPicked, setDraftAutoPicked] = useState(false);
  const draftSelections = useMemo<ResolutionSelection[]>(
    () =>
      (draft ?? []).map((item: CreateDraftItem) => ({
        canonical_id: item.canonical_id,
        kind: item.kind,
        version_id: item.version_id ?? null,
      })),
    [draft],
  );

  useEffect(() => {
    if (!openDraft) return;
    let cancelled = false;
    contentCompatibility(draftSelections)
      .then((response) => {
        if (cancelled) return;
        const best = response.candidates[0];
        if (best) setSourceId(sourceIdFromLoaderKey(loaderKeyFromWire(best.loader)));
        setDraftCandidates(response.candidates);
        if (!best) setViewLoading(false);
      })
      .catch((err: unknown) => {
        if (!cancelled) setDraftError(errMessage(err));
      });
    return () => {
      cancelled = true;
    };
  }, [openDraft, draftSelections]);

  const draftCandidateFor = (loader: LoaderKey, mcVersion: string): CompatCandidate | null =>
    draftCandidates?.find(
      (candidate) => loaderKeyFromWire(candidate.loader) === loader && candidate.game_version === mcVersion,
    ) ?? null;

  const draftLoaders = useMemo(() => {
    if (!openDraft || !draftCandidates) return null;
    return new Set(draftCandidates.map((candidate) => loaderKeyFromWire(candidate.loader)));
  }, [openDraft, draftCandidates]);

  const [screenMax, setScreenMax] = useState<ScreenSize>(() => ({
    w: typeof window !== 'undefined' && window.screen ? window.screen.width : 1920,
    h: typeof window !== 'undefined' && window.screen ? window.screen.height : 1080,
  }));
  useEffect(() => {
    let cancelled = false;
    void detectMaxScreenSize().then((s) => {
      if (!cancelled) setScreenMax(s);
    });
    return () => {
      cancelled = true;
    };
  }, []);
  const windowPresets: WindowPresetSpec[] = useMemo(() => buildWindowPresets(screenMax), [screenMax]);
  const [windowPresetId, setWindowPresetId] = useState<string>('default');
  useEffect(() => {
    if (!windowPresets.some((p) => p.id === windowPresetId)) {
      setWindowPresetId('default');
    }
  }, [windowPresets, windowPresetId]);

  const applyCreateView = (source: CreateSourceId, res: CreateView, applyMutableDefaults = true): void => {
    loadedCreateViewSourceRef.current = source;
    setAppliedSource(source);
    setBackendView(res);
    const options = res.preset_options;
    setPresetOptions(options);
    const selectableOptions = options;
    const defaultPresetId = res.defaults.jvm_preset_id;
    const defaultOption =
      selectableOptions.find((option) => option.id === defaultPresetId) ??
      selectableOptions.find((option) => option.default) ??
      selectableOptions[0];
    if (defaultOption) {
      setJvmPreset((current) =>
        selectableOptions.some((option) => option.id === current) ? current : defaultOption.id,
      );
    }
    if (applyMutableDefaults) {
      const defaultMaxMemoryMb = res.defaults.max_memory_mb;
      if (typeof defaultMaxMemoryMb === 'number' && Number.isFinite(defaultMaxMemoryMb) && defaultMaxMemoryMb > 0) {
        setMemoryGB(Math.max(1, Math.round((defaultMaxMemoryMb / 1024) * 2) / 2));
      }
      const defaultWindowWidth = res.defaults.window_width;
      const defaultWindowHeight = res.defaults.window_height;
      if (
        typeof defaultWindowWidth === 'number' &&
        Number.isFinite(defaultWindowWidth) &&
        defaultWindowWidth > 0 &&
        typeof defaultWindowHeight === 'number' &&
        Number.isFinite(defaultWindowHeight) &&
        defaultWindowHeight > 0
      ) {
        const defaultWindowPreset = windowPresets.find(
          (preset) => preset.w === defaultWindowWidth && preset.h === defaultWindowHeight,
        );
        setWindowPresetId(defaultWindowPreset?.id ?? 'default');
      }
    }
    const defaultSource = res.defaults.source_id;
    const sources = res.sources.filter((option) => option.enabled);
    const nextSource = sources.find((option) => option.id === defaultSource) ?? sources[0];
    if (nextSource)
      setSourceId((current) =>
        sources.some((option) => option.id === current) ? current : (nextSource.id as CreateSourceId),
      );
    const defaultChannel = normalizeChannel(res.defaults.channel_id);
    setChannel((current) => (CHANNEL_ORDER.includes(current) ? current : defaultChannel));
  };

  const loadCreateView = async (source: CreateSourceId = sourceId, providedView?: CreateView): Promise<void> => {
    const requestId = loadRequestRef.current + 1;
    loadRequestRef.current = requestId;
    if (providedView) {
      storeView(source, providedView);
      applyCreateView(source, providedView);
      setViewLoading(false);
      setViewError(null);
      return;
    }
    const cached = cachedView(source);
    if (cached) applyCreateView(source, cached);
    setViewLoading(!cached);
    setViewError(null);
    try {
      const res = await fetchView(source);
      if (requestId !== loadRequestRef.current) return;
      applyCreateView(source, res, !cached);
    } catch (err: unknown) {
      if (requestId !== loadRequestRef.current) return;
      // A failed background refresh keeps showing the cached view.
      if (!cached) {
        if (loadedCreateViewSourceRef.current === source) loadedCreateViewSourceRef.current = null;
        setBackendView(null);
        setPresetOptions([]);
        setViewError(errMessage(err));
      }
    } finally {
      if (requestId === loadRequestRef.current) setViewLoading(false);
    }
  };

  useEffect(() => {
    if (openDraft && draftCandidates === null && !draftError) return;
    if (openDraft && draftCandidates?.length === 0) return;
    if (openDraft && loadedCreateViewSourceRef.current === sourceId) return;
    void loadCreateView(sourceId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sourceId, draftCandidates, draftError]);

  const sourceOptions = useMemo<CreateOption[]>(() => backendView?.sources ?? [], [backendView]);
  const channelOptions = useMemo<CreateOption[]>(() => backendView?.channels ?? [], [backendView]);

  useEffect(() => {
    if (sourceOptions.some((option) => option.id === sourceId)) return;
    const fallback = sourceOptions.find((option) => option.enabled);
    if (fallback) setSourceId(fallback.id as CreateSourceId);
  }, [sourceOptions, sourceId]);

  const backendRows = useMemo<CreateVersionRow[]>(() => backendView?.versions ?? [], [backendView]);
  const notices = useMemo<CreateNotice[]>(() => backendView?.notices ?? [], [backendView]);

  const availableForSource = useMemo(
    () => backendRows.filter((row) => row.source_id === sourceId),
    [backendRows, sourceId],
  );

  // Staged picks narrow every loader's versions to the ones they all support.
  const draftAllows = useMemo(() => {
    if (!openDraft || !draftCandidates) return () => true;
    return (row: CreateVersionRow) => {
      const loader = loaderKeyFromSourceId(row.source_id);
      return draftCandidates.some(
        (candidate) =>
          loaderKeyFromWire(candidate.loader) === loader && candidate.game_version === row.minecraft_version_id,
      );
    };
  }, [openDraft, draftCandidates]);

  const sourceRows = useMemo<VersionRowModel[]>(
    () => availableForSource.filter(draftAllows).map(versionRow),
    [availableForSource, draftAllows],
  );

  const availableChannels = useMemo<Channel[]>(
    () => CHANNEL_ORDER.filter((channel) => sourceRows.some((row) => row.channel === channel)),
    [sourceRows],
  );

  useEffect(() => {
    if (availableChannels.length === 0) return;
    if (availableChannels.includes(channel)) return;
    setChannel(availableChannels[0]!);
  }, [availableChannels, channel]);

  // A search reaches every channel, and the loader its letters name.
  const found = useVersionSearch(query, {
    source: sourceId,
    allows: draftAllows,
    offers: (loader) => !draftLoaders || draftLoaders.has(loader),
    limit: SEARCH_LIMIT,
  });
  const versionRows = found ? found.rows : sourceRows.filter((row) => row.channel === channel);
  const selectedVersionRow = selectedSelectionId
    ? (sourceRows.find((row) => row.selectionId === selectedSelectionId) ?? null)
    : null;
  const mcVersionId = pack ? pack.minecraft : (selectedVersionRow?.id ?? null);
  const identityKey = pack ? loaderKeyFromWire(pack.loader) : sourceKey;
  const selectionId = pack ? pack.selection_id : (selectedVersionRow?.selectionId ?? '');

  const channelTabs = useMemo<Channel[]>(() => {
    const backendTabs = channelOptions.map((option) => normalizeChannel(option.id));
    const tabs =
      backendTabs.length > 0
        ? [...backendTabs, ...availableChannels.filter((c) => !backendTabs.includes(c))]
        : availableChannels;
    // Unclassified versions get a tab only where there are some.
    return tabs.filter(
      (value, index, values) =>
        values.indexOf(value) === index && (value !== 'unknown' || availableChannels.includes(value)),
    );
  }, [channelOptions, availableChannels]);

  useEffect(() => {
    if (!openDraft || !draftCandidates || draftAutoPicked) return;
    const best = draftCandidates[0];
    if (!best) return;
    const bestKey = loaderKeyFromWire(best.loader);
    if (sourceKey !== bestKey) return;
    const row = sourceRows.find((entry) => entry.id === best.game_version && entry.createEnabled);
    if (row) {
      setSelectedSelectionId(row.selectionId);
      setChannel(row.channel);
      setDraftAutoPicked(true);
    }
  }, [openDraft, draftCandidates, draftAutoPicked, sourceKey, sourceRows]);

  const draftCandidate = openDraft && selectedVersionRow ? draftCandidateFor(sourceKey, selectedVersionRow.id) : null;
  // With a pack the picks were already checked against its loader and version, where they were staged.
  const picked = pack && draft ? staging.value : null;
  const packDrops = picked?.packs.find((entry) => entry.item.canonical_id === pack?.canonical_id)?.fit?.drops;
  const draftDrops = useMemo(
    () =>
      pack
        ? (draft ?? [])
            .filter((item) => packDrops?.has(item.canonical_id))
            .map((item) => ({ canonical_id: item.canonical_id, title: item.title }))
        : (draftCandidate?.drops ?? []),
    [pack, draft, packDrops, draftCandidate],
  );
  const draftDropped = useMemo(() => new Set(draftDrops.map((drop) => drop.canonical_id)), [draftDrops]);

  const suggestedName = useMemo(() => {
    if (pack) return pack.name;
    if (!mcVersionId) return '';
    return defaultNameFor(sourceKey, mcVersionId);
  }, [pack, sourceKey, mcVersionId]);

  const name = nameOverride ?? suggestedName;
  const displayName = name.trim() || suggestedName || 'New instance';

  const baseSeed = useMemo(() => {
    const previewId = `preview:${sourceId}:${mcVersionId ?? 'none'}`;
    return hashStr(`${previewId}:${displayName}:${mcVersionId ?? 'preview'}`) || 1;
  }, [sourceId, mcVersionId, displayName]);
  const paletteSeed = paletteSeedOverride ?? baseSeed;
  const tileHues = useMemo(() => harmoniousTilePalette(theme, paletteSeed), [theme, paletteSeed]);
  const previewSeed = seedForTileHue(tileHues[paletteSeed % tileHues.length] ?? theme.hue + 180);
  const previewInstance = {
    id: `preview:${sourceId}:${mcVersionId ?? 'none'}`,
    name: displayName,
    version_id: mcVersionId ?? '',
    art_seed: previewSeed,
    version_display: { loader_key: identityKey },
  };
  const shuffleColor = (): void => {
    Sound.ui('click');
    setPaletteSeedOverride(nextArtSeed(paletteSeed));
  };

  useEffect(() => {
    if (!selectedSelectionId) return;
    if (sourceRows.some((row) => row.selectionId === selectedSelectionId)) return;
    setSelectedSelectionId(null);
  }, [sourceRows, selectedSelectionId]);

  const draftReady = picked ? Boolean(packDrops) : !openDraft || draftCandidates !== null;
  const versionReady = (pack ? true : Boolean(selectionId && selectedVersionRow?.createEnabled === true)) && draftReady;
  const currentLoaderBuilds =
    loaderBuilds?.source_id === sourceId && loaderBuilds.minecraft_version_id === mcVersionId ? loaderBuilds : null;
  const createSelectionId = pack
    ? pack.selection_id
    : createLoaderSelection(currentLoaderBuilds, sourceId, mcVersionId, selectionId, loaderChoice);
  const canCreate = versionReady && Boolean(createSelectionId) && name.trim().length > 0 && !submitting;

  useEffect(() => {
    setLoaderBuilds(null);
    setLoaderBuildsError(null);
    setLoaderChoice(null);
    if (pack || sourceKey === 'vanilla' || !mcVersionId) return;
    let cancelled = false;
    fetchLoaderBuilds(sourceId, mcVersionId)
      .then((res) => {
        if (cancelled) return;
        if (res.source_id !== sourceId || res.minecraft_version_id !== mcVersionId) {
          throw new Error('Loader options did not match the selected Minecraft version.');
        }
        setLoaderBuilds(res);
      })
      .catch((err: unknown) => {
        if (!cancelled) setLoaderBuildsError(errMessage(err));
      });
    return () => {
      cancelled = true;
    };
  }, [sourceKey, sourceId, mcVersionId]);

  const loaderOptions = useMemo(() => {
    if (!currentLoaderBuilds) return [];
    return [
      {
        value: currentLoaderBuilds.auto.selection_id,
        label: currentLoaderBuilds.auto.label,
        disabled: !currentLoaderBuilds.auto.enabled,
      },
      ...currentLoaderBuilds.builds.map((build) => {
        const extras = [
          build.channel_label,
          build.recommended ? 'Recommended' : null,
          build.installed ? 'Installed' : null,
        ].filter(Boolean);
        return {
          value: build.selection_id,
          label: extras.length > 0 ? `${build.label} (${extras.join(', ')})` : build.label,
          disabled: !build.enabled,
        };
      }),
    ];
  }, [currentLoaderBuilds]);

  const optimizeOption = backendView?.optimize_option ?? null;
  const autoOptimize = optimizeChoice ?? optimizeOption?.default_enabled ?? false;
  const windowPreset = windowPresets.find((p) => p.id === windowPresetId) ??
    windowPresets[windowPresets.length - 1] ?? { id: 'default', label: 'Default', w: 0, h: 0 };
  const presetOption =
    presetOptions.find((option) => option.id === jvmPreset) ?? presetOptions.find((option) => option.default) ?? null;

  const submit = async (quickly = false): Promise<void> => {
    if (submittingRef.current || !canCreate) return;
    const trimmed = name.trim();
    if (!trimmed || !createSelectionId) return;
    submittingRef.current = true;
    setSubmitting(true);
    try {
      const keptSelections = draft
        ? draftSelections.filter((selection) => !draftDropped.has(selection.canonical_id))
        : [];
      let setupPlanId: string | undefined;
      let setupSelectionId: string | undefined;
      if (openDraft && mcVersionId && keptSelections.length > 0) {
        try {
          const setup = await planInstanceSetup(
            createSelectionId,
            {
              kind: 'draft',
              ...(sourceKey === 'vanilla' ? {} : { loader: sourceKey }),
              game_version: mcVersionId,
            },
            keptSelections,
          );
          if (setup.plan.conflicts.length > 0 || !setup.plan_id) {
            const detail = setup.plan.conflicts[0]?.detail ?? 'No installable setup is available.';
            toast(`Cannot create this setup: ${detail}`, 'error');
            return;
          }
          setupPlanId = setup.plan_id;
          setupSelectionId = setup.selection_id;
        } catch (error) {
          toast(`Could not validate this setup: ${errMessage(error)}`, 'error');
          return;
        }
      }
      const winSpec = windowPresets.find((p) => p.id === windowPresetId);
      const dims = winSpec && winSpec.id !== 'default' ? { w: winSpec.w, h: winSpec.h } : null;
      const result = await createInstance({
        name: trimmed,
        selectionId: setupSelectionId ?? createSelectionId,
        icon: defaultIconFor(identityKey),
        accent: config.value?.theme ?? '',
        setupPlanId,
        ...(pack ? { modpack: { canonicalId: pack.canonical_id, versionId: pack.version_id } } : {}),
        initialSettings: {
          max_memory_mb: Math.round(memoryGB * 1024),
          art_seed: previewSeed,
          ...(dims ? { window_width: dims.w, window_height: dims.h } : {}),
          jvm_preset_id: jvmPreset,
          ...(optimizeOption ? { auto_optimize: autoOptimize } : {}),
        },
      });
      if (!result.ok) return;
      if (!pack && !draft) {
        if (quickly) noteHabit('create.quick');
        else {
          noteHabit('create.by-hand');
          // Said once, to someone who keeps making instances the long way.
          if (habit('create.by-hand') >= 3 && habit('create.quick') === 0 && firstTime('tip.quick-create')) {
            toast('Tip: type "forge 1.13.1" in the Minecraft field and press Enter to create it.', 'info', {
              duration: 12_000,
            });
          }
        }
      }
      if (result.instance) {
        const held = pack && draft ? await addPicksTo(result.instance.id, draft, draftDropped) : [];
        unstageAll([
          ...(pack?.pick ? [pack.pick] : []),
          ...(draft ?? []).filter((item) => !held.includes(item)).map(selectionOf),
        ]);
      }
      closeCreate();
    } finally {
      submittingRef.current = false;
      setSubmitting(false);
    }
  };

  const chooseVersion = (row: VersionRowModel): void => {
    if (row.sourceId !== sourceId) {
      // Found through search, so its view is cached: apply it with the switch, leaving no render without rows.
      const view = cachedView(row.sourceId);
      if (view) applyCreateView(row.sourceId, view, false);
      setSourceId(row.sourceId);
    }
    // Another version's builds are fetched afresh; their failure no longer speaks for this one.
    if (row.sourceId !== sourceId || row.id !== mcVersionId) setLoaderBuildsError(null);
    setSelectedSelectionId(row.selectionId);
    setChannel(row.channel);
    setQuery('');
  };

  const awaitingCreate =
    viewLoading ||
    !draftReady ||
    (sourceKey !== 'vanilla' && mcVersionId != null && !currentLoaderBuilds && !loaderBuildsError);
  useEffect(() => {
    if (!queuedCreate) return;
    if (queuedCreate !== selectedSelectionId) setQueuedCreate(null);
    else if (appliedSource !== sourceId) return;
    else if (canCreate) {
      setQueuedCreate(null);
      void submit(true);
    } else if (!awaitingCreate) setQueuedCreate(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [queuedCreate, selectedSelectionId, appliedSource, canCreate, awaitingCreate]);

  return {
    pack,
    draft,
    source: {
      id: sourceId,
      key: sourceKey,
      label: pack
        ? pack.loader_label
        : (sourceOptions.find((option) => option.id === sourceId)?.label ?? LOADER_LABELS[sourceKey]),
      options: sourceOptions,
      unsupported: (key: LoaderKey) => draftLoaders !== null && !draftLoaders.has(key),
      choose: (id: CreateSourceId) => {
        setSourceId(id);
        setSelectedSelectionId(null);
      },
    },
    identityKey,
    versions: {
      rows: versionRows,
      selected: selectedVersionRow,
      choose: chooseVersion,
      /** Choose it and create as soon as it can be. */
      create: (row: VersionRowModel) => {
        chooseVersion(row);
        setQueuedCreate(row.selectionId);
      },
      query,
      setQuery,
      searching: found !== null,
      /** The loaders named are still being listed, so the top match may yet change. */
      searchPending: found?.pending ?? false,
      /** The search named a loader, which nobody types by accident. */
      loaderNamed: found?.loader != null,
      /** Rows come from more than the chosen loader and say which one they are. */
      acrossLoaders: versionRows.some((row) => row.sourceId !== sourceId),
      channel,
      setChannel,
      channelTabs,
      availableChannels,
      loading: viewLoading,
      error: viewError,
      reload: () => void loadCreateView(),
      compatibility: (row: VersionRowModel) =>
        openDraft && draftCandidates ? draftCandidateFor(loaderKeyFromSourceId(row.sourceId), row.id) : null,
    },
    loaderBuild: {
      shown: !pack && sourceKey !== 'vanilla' && mcVersionId != null,
      loaded: currentLoaderBuilds,
      error: loaderBuildsError,
      options: loaderOptions,
      value: loaderChoice ?? currentLoaderBuilds?.auto.selection_id ?? '',
      choose: (next: string) => setLoaderChoice(next === currentLoaderBuilds?.auto.selection_id ? null : next),
      title: !loaderChoice ? currentLoaderBuilds?.auto.disabled_reason || undefined : undefined,
    },
    identity: {
      name,
      placeholder: suggestedName || 'New instance',
      rename: setNameOverride,
      preview: previewInstance,
      shuffleColor,
    },
    staged: {
      dropped: draftDropped,
      drops: draftDrops,
      error: picked ? picked.error : draftError,
      /** Ask again what the pack's instance takes, when that check failed. */
      retry: picked ? recheck : undefined,
      candidates: draftCandidates,
    },
    options: {
      memoryGB,
      setMemoryGB,
      totalGB,
      memoryRec: memoryRec.rec,
      windowPresets,
      windowPreset,
      setWindowPresetId,
      presetOptions,
      presetOption,
      jvmPreset,
      setJvmPreset,
      optimizeOption,
      autoOptimize,
      toggleOptimize: () => {
        Sound.ui('click');
        setOptimizeChoice(!autoOptimize);
      },
    },
    notices,
    canCreate,
    submitting: submitting || queuedCreate !== null,
    submit,
  };
}

export type CreateForm = ReturnType<typeof useCreateForm>;
