import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { nextArtSeed } from '../../ui/InstanceVisual';
import { harmoniousTilePalette, seedForTileHue } from '../../ui/look-guardian';
import { useTheme } from '../../hooks/use-theme';
import { config, systemInfo } from '../../store';
import { closeCreate, createDraft, createModpack, type CreateDraftItem } from '../../ui-state';
import { api } from '../../api';
import { contentCompatibility, planInstanceSetup } from '../../content';
import { toast } from '../../toast';
import type { CompatCandidate, ContentSelection } from '../../types-content';
import { recheck, staging } from '../discover/fit';
import { addPicksTo } from '../discover/make';
import { selectionOf, unstageAll } from '../discover/state';
import { errMessage, getMemoryRecommendation } from '../../utils';
import { hashStr } from '../../tokens';
import { Sound } from '../../sound';
import { createInstance } from '../../instance-create';
import type { CreateNotice } from '../../create-presenters';
import {
  buildWindowPresets,
  detectMaxScreenSize,
  type ScreenSize,
  type WindowPresetSpec,
} from '../../ui/screen-presets';
import {
  defaultIconFor,
  defaultNameFor,
  LOADER_COMPONENT_IDS,
  LOADER_LABELS,
  loaderKeyFromComponentId,
  type Channel,
  type LoaderKey,
} from './defaults';
import { CHANNEL_ORDER, type VersionDownloadState, type VersionRowModel } from './view-model';
import {
  createBackendViewResponse,
  createLoaderBuildsResponse,
  createLoaderSelection,
  type CreateBackendViewResponse,
  type CreateLoaderBuildsResponse,
  type CreateOption,
  type CreatePresetOption,
  type CreateSourceId,
  type CreateVersionRow,
} from './contract';

/* Last good create-view response per source, kept across dialog opens so the
   version list renders immediately while a fresh copy loads in the background. */
const createViewCache = new Map<CreateSourceId, CreateBackendViewResponse>();

export function loaderKeyFromSourceId(sourceId: CreateSourceId): LoaderKey {
  if (sourceId === 'vanilla') return 'vanilla';
  return loaderKeyFromComponentId(sourceId);
}

function sourceIdFromLoaderKey(loader: LoaderKey): CreateSourceId {
  return loader === 'vanilla' ? 'vanilla' : LOADER_COMPONENT_IDS[loader];
}

function loaderKeyFromWire(value: string | null | undefined): LoaderKey {
  if (!value || value === 'vanilla') return 'vanilla';
  if (value === 'fabric' || value === 'quilt' || value === 'forge' || value === 'neoforge') return value;
  throw new Error('Create loader response was invalid.');
}

function normalizeChannel(value: string | undefined): Channel {
  return CHANNEL_ORDER.includes(value as Channel) ? (value as Channel) : 'unknown';
}

function normalizeDownloadState(value: string | undefined): VersionDownloadState {
  return value === 'base' || value === 'full' ? value : 'none';
}

function rowSearchText(row: VersionRowModel): string {
  return [row.id, row.displayName, row.hint ?? '', row.tags.map((tag) => tag.label).join(' ')].join(' ').toLowerCase();
}

/** The create form's one state owner: source, version, identity, options, staged picks and submission. */
export function useCreateForm() {
  const pack = createModpack.value;
  const draft = createDraft.value;
  // Picks alone choose their loader and version; with a pack they follow the pack's.
  const openDraft = pack ? null : draft;
  const theme = useTheme();
  const [sourceId, setSourceId] = useState<CreateSourceId>('vanilla');
  const [selectedSelectionId, setSelectedSelectionId] = useState<string | null>(null);
  const [channel, setChannel] = useState<Channel>('release');
  const [query, setQuery] = useState('');
  const [nameOverride, setNameOverride] = useState<string | null>(null);
  const [viewError, setViewError] = useState<string | null>(null);
  const [viewLoading, setViewLoading] = useState(Boolean(openDraft));
  const [backendView, setBackendView] = useState<CreateBackendViewResponse | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const submittingRef = useRef(false);
  const loadRequestRef = useRef(0);
  const loadedCreateViewSourceRef = useRef<string | null>(null);
  const sourceKey = loaderKeyFromSourceId(sourceId);

  const totalGB = systemInfo.value?.total_memory_mb ? Math.floor(systemInfo.value.total_memory_mb / 1024) : 16;
  const memoryRec = getMemoryRecommendation(totalGB);
  const [memoryGB, setMemoryGB] = useState<number>(memoryRec.rec);
  const [paletteSeedOverride, setPaletteSeedOverride] = useState<number | null>(null);
  const [jvmPreset, setJvmPreset] = useState('');
  const [presetOptions, setPresetOptions] = useState<CreatePresetOption[]>([]);
  const [optimizeChoice, setOptimizeChoice] = useState<boolean | null>(null);
  const [loaderChoice, setLoaderChoice] = useState<string | null>(null);
  const [loaderBuilds, setLoaderBuilds] = useState<CreateLoaderBuildsResponse | null>(null);
  const [loaderBuildsError, setLoaderBuildsError] = useState<string | null>(null);

  const [draftCandidates, setDraftCandidates] = useState<CompatCandidate[] | null>(null);
  const [draftError, setDraftError] = useState<string | null>(null);
  const [draftAutoPicked, setDraftAutoPicked] = useState(false);
  const draftSelections = useMemo<ContentSelection[]>(
    () =>
      (draft ?? []).map((item: CreateDraftItem) => ({
        canonical_id: item.canonical_id,
        kind: item.kind,
        ...(item.version_id ? { version_id: item.version_id } : {}),
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
        if (best) {
          const source = sourceIdFromLoaderKey(loaderKeyFromWire(best.loader));
          setSourceId(source);
          if (response.create_view) {
            void loadCreateView(source, createBackendViewResponse(response.create_view));
          }
        }
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

  const applyCreateView = (source: string, res: CreateBackendViewResponse, applyMutableDefaults = true): void => {
    loadedCreateViewSourceRef.current = source;
    setBackendView(res);
    const options = res.preset_options;
    setPresetOptions(options);
    const selectableOptions = options.filter((option) => !option.disabled_reason);
    const defaultPresetId = res.defaults?.jvm_preset_id;
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
      const defaultMaxMemoryMb = res.defaults?.max_memory_mb;
      if (typeof defaultMaxMemoryMb === 'number' && Number.isFinite(defaultMaxMemoryMb) && defaultMaxMemoryMb > 0) {
        setMemoryGB(Math.max(1, Math.round((defaultMaxMemoryMb / 1024) * 2) / 2));
      }
      const defaultWindowWidth = res.defaults?.window_width;
      const defaultWindowHeight = res.defaults?.window_height;
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
    const defaultSource = res.defaults?.source_id;
    const sources = Array.isArray(res.sources) ? res.sources.filter((option) => option.enabled) : [];
    const nextSource = sources.find((option) => option.id === defaultSource) ?? sources[0];
    if (nextSource)
      setSourceId((current) => (sources.some((option) => option.id === current) ? current : nextSource.id));
    const defaultChannel = normalizeChannel(res.defaults?.channel_id);
    setChannel((current) => (CHANNEL_ORDER.includes(current) ? current : defaultChannel));
  };

  const loadCreateView = async (
    source: CreateSourceId = sourceId,
    providedView?: CreateBackendViewResponse,
  ): Promise<void> => {
    const requestId = loadRequestRef.current + 1;
    loadRequestRef.current = requestId;
    if (providedView) {
      createViewCache.set(source, providedView);
      applyCreateView(source, providedView);
      setViewLoading(false);
      setViewError(null);
      return;
    }
    const cached = createViewCache.get(source);
    if (cached) applyCreateView(source, cached);
    setViewLoading(!cached);
    setViewError(null);
    try {
      const res = createBackendViewResponse(
        await api('GET', `/instances/create-view?source=${encodeURIComponent(source)}`),
      );
      if (requestId !== loadRequestRef.current) return;
      createViewCache.set(source, res);
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

  const sourceOptions = useMemo<CreateOption<CreateSourceId>[]>(() => backendView?.sources ?? [], [backendView]);
  const channelOptions = useMemo<CreateOption[]>(() => backendView?.channels ?? [], [backendView]);

  useEffect(() => {
    if (sourceOptions.some((option) => option.id === sourceId)) return;
    const fallback = sourceOptions.find((option) => option.enabled);
    if (fallback) setSourceId(fallback.id);
  }, [sourceOptions, sourceId]);

  const backendRows = useMemo<CreateVersionRow[]>(() => backendView?.versions ?? [], [backendView]);
  const notices = useMemo<CreateNotice[]>(() => backendView?.notices ?? [], [backendView]);

  const availableForSource = useMemo(
    () => backendRows.filter((row) => row.source_id === sourceId),
    [backendRows, sourceId],
  );

  const compatibleForSource = useMemo(() => {
    if (!openDraft || !draftCandidates) return availableForSource;
    return availableForSource.filter((row) =>
      draftCandidates.some(
        (candidate) =>
          loaderKeyFromWire(candidate.loader) === sourceKey && candidate.game_version === row.minecraft_version_id,
      ),
    );
  }, [availableForSource, openDraft, draftCandidates, sourceKey]);

  const availableChannels = useMemo<Channel[]>(() => {
    const has: Record<Channel, boolean> = { release: false, snapshot: false, legacy: false, unknown: false };
    for (const row of compatibleForSource) has[normalizeChannel(row.channel)] = true;
    return CHANNEL_ORDER.filter((c) => has[c]);
  }, [compatibleForSource]);

  useEffect(() => {
    if (availableChannels.length === 0) return;
    if (availableChannels.includes(channel)) return;
    setChannel(availableChannels[0]!);
  }, [availableChannels, channel]);

  const versionRows: VersionRowModel[] = useMemo(() => {
    const q = query.trim().toLowerCase();
    return compatibleForSource
      .map((row) => ({
        id: row.minecraft_version_id,
        selectionId: row.selection_id,
        displayName: row.display_name,
        hint: row.hint ?? null,
        channel: normalizeChannel(row.channel),
        tags: row.tags,
        downloadState: normalizeDownloadState(row.download_state),
        createEnabled: row.create_enabled,
        disabledReason: row.disabled_reason ?? null,
      }))
      .filter((row) => row.channel === channel)
      .filter((row) => !q || rowSearchText(row).includes(q));
  }, [channel, query, compatibleForSource]);
  const selectedVersionRow = selectedSelectionId
    ? (versionRows.find((row) => row.selectionId === selectedSelectionId) ?? null)
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
    return tabs.filter((value, index, values) => values.indexOf(value) === index);
  }, [channelOptions, availableChannels]);

  useEffect(() => {
    if (!openDraft || !draftCandidates || draftAutoPicked) return;
    const best = draftCandidates[0];
    if (!best) return;
    const bestKey = loaderKeyFromWire(best.loader);
    if (sourceKey !== bestKey) return;
    const row = versionRows.find((entry) => entry.id === best.game_version && entry.createEnabled);
    if (row) {
      setSelectedSelectionId(row.selectionId);
      setDraftAutoPicked(true);
    }
  }, [openDraft, draftCandidates, draftAutoPicked, sourceKey, versionRows]);

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
    if (versionRows.some((row) => row.selectionId === selectedSelectionId)) return;
    setSelectedSelectionId(null);
  }, [versionRows, selectedSelectionId]);

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
    api(
      'GET',
      `/instances/create-view/loader-builds?source=${encodeURIComponent(sourceId)}&minecraft_version=${encodeURIComponent(mcVersionId)}`,
    )
      .then((value) => createLoaderBuildsResponse(value, { sourceId, minecraftVersionId: mcVersionId }))
      .then((res) => {
        if (cancelled) return;
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

  const submit = async (): Promise<void> => {
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
      choose: setSelectedSelectionId,
      query,
      setQuery,
      channel,
      setChannel,
      channelTabs,
      availableChannels,
      loading: viewLoading,
      error: viewError,
      reload: () => void loadCreateView(),
      compatibility: (row: VersionRowModel) =>
        openDraft && draftCandidates ? draftCandidateFor(sourceKey, row.id) : null,
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
    submitting,
    submit,
  };
}

export type CreateForm = ReturnType<typeof useCreateForm>;
