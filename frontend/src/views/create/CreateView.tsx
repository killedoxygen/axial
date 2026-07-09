import type { JSX } from 'preact';
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, IconButton, Input, Pill, Toggle } from '../../ui/Atoms';
import { Segmented } from '../../ui/Segmented';
import { Icon } from '../../ui/Icons';
import { Slider } from '../../ui/Slider';
import { SelectField } from '../../ui/Select';
import { InstanceTile, nextArtSeed } from '../../ui/InstanceVisual';
import { harmoniousTilePalette, seedForTileHue } from '../../ui/look-guardian';
import { useTheme } from '../../hooks/use-theme';
import { config, systemInfo } from '../../store';
import { closeCreate, createOpen } from '../../ui-state';
import { api } from '../../api';
import { errMessage, fmtMem, getMemoryRecommendation } from '../../utils';
import { hashStr } from '../../tokens';
import { Sound } from '../../sound';
import { createInstance } from '../../instance-create';
import type { LoaderComponentId } from '../../types-loader';
import {
  defaultIconFor,
  defaultNameFor,
  LOADER_LABELS,
  LOADER_TAGLINES,
  loaderKeyFromComponentId,
  type Channel,
  type LoaderKey,
} from './defaults';
import { LoaderLogo } from './loader-logos';
import { Modal, ModalContent } from '../../ui/Modal';
import {
  CHANNEL_ORDER,
  CHANNEL_LABEL,
  type VersionDownloadState,
  type VersionRowModel,
  type VersionRowTagModel,
} from './view-model';
import {
  buildWindowPresets,
  detectMaxScreenSize,
  nextWindowPreset,
  type ScreenSize,
  type WindowPresetSpec,
} from './screen-presets';

type CreateStep = 'version' | 'details';

interface CreatePresetOption {
  id: string;
  label: string;
  detail: string;
  default: boolean;
  disabled_reason?: string | null;
}

interface CreateOption {
  id: string;
  label: string;
  enabled: boolean;
  disabled_reason?: string | null;
}

interface CreateVersionRow {
  source_id: string;
  selection_id: string;
  minecraft_version_id: string;
  loader_build?: CreateLoaderBuildIdentity | null;
  display_name: string;
  hint?: string | null;
  channel: string;
  tags?: CreateVersionTag[];
  download_state: string;
  create_enabled: boolean;
  disabled_reason?: string | null;
}

interface CreateVersionTag {
  id?: string;
  label?: string;
}

interface CreateLoaderBuildIdentity {
  component_id: LoaderComponentId;
  build_id: string;
  target_version_id: string;
  minecraft_version_id: string;
  loader_version: string;
  installability: string;
  availability: {
    fresh: boolean;
    stale: boolean;
    cache_hit: boolean;
    checked_at_ms: number;
    last_success_at_ms?: number | null;
    last_error?: string | null;
    last_failure_kind?: string | null;
  };
}

interface CreateNotice {
  state_id: string;
  tone: string;
  message: string;
  detail?: string | null;
}

interface CreateOptimizeOption {
  id: string;
  label: string;
  detail: string;
  default_enabled: boolean;
}

interface CreateLoaderAutoOption {
  selection_id: string;
  label: string;
  detail: string;
}

interface CreateLoaderBuildOption {
  selection_id: string;
  build_id: string;
  label: string;
  channel_id: string;
  channel_label: string;
  recommended: boolean;
  installed: boolean;
  enabled: boolean;
  disabled_reason?: string | null;
}

interface CreateLoaderBuildsResponse {
  source_id: string;
  minecraft_version_id: string;
  auto: CreateLoaderAutoOption;
  builds: CreateLoaderBuildOption[];
}

interface CreateBackendViewResponse {
  sources?: CreateOption[];
  channels?: CreateOption[];
  versions?: CreateVersionRow[];
  preset_options?: CreatePresetOption[];
  optimize_option?: CreateOptimizeOption;
  notices?: CreateNotice[];
  defaults?: {
    source_id?: string;
    channel_id?: string;
    jvm_preset_id?: string;
    max_memory_mb?: number | null;
    window_width?: number | null;
    window_height?: number | null;
  };
}

export function CreateView(): JSX.Element {
  return (
    <Modal
      open={createOpen.value}
      onOpenChange={(next: boolean) => {
        if (!next) closeCreate();
      }}
    >
      <ModalContent
        className="cp-cr-card"
        aria-label="Create instance"
        aria-describedby={undefined}
        showCloseButton={false}
      >
        <CreateCard />
      </ModalContent>
    </Modal>
  );
}

function versionStatusTitle(state: VersionDownloadState, source: LoaderKey): string {
  if (state === 'full') return 'Already installed';
  if (state === 'base') {
    return source === 'vanilla'
      ? 'Already installed'
      : 'Base Minecraft version is installed; loader still needs download';
  }
  return '';
}

function loaderKeyFromSourceId(sourceId: string): LoaderKey {
  if (sourceId === 'vanilla') return 'vanilla';
  return loaderKeyFromComponentId(sourceId as LoaderComponentId);
}

function normalizeChannel(value: string | undefined): Channel {
  return CHANNEL_ORDER.includes(value as Channel) ? (value as Channel) : 'unknown';
}

function normalizeDownloadState(value: string | undefined): VersionDownloadState {
  return value === 'base' || value === 'full' ? value : 'none';
}

function noticeTone(value: string): string {
  if (value === 'warn' || value === 'warned') return 'warned';
  if (value === 'error') return 'error';
  if (value === 'intervened') return 'intervened';
  if (value === 'success') return 'success';
  return 'info';
}

function noticeIcon(tone: string): string {
  if (tone === 'success') return 'check-circle';
  if (tone === 'error' || tone === 'warned') return 'alert';
  if (tone === 'intervened') return 'shield-check';
  return 'info';
}

function normalizeVersionTags(tags: CreateVersionTag[] | undefined): VersionRowTagModel[] {
  return Array.isArray(tags)
    ? tags
        .filter((tag): tag is Required<CreateVersionTag> => {
          return typeof tag.id === 'string' && typeof tag.label === 'string';
        })
        .map((tag) => ({ id: tag.id, label: tag.label }))
    : [];
}

function rowSearchText(row: VersionRowModel): string {
  return [row.id, row.displayName, row.hint ?? '', row.tags.map((tag) => tag.label).join(' ')].join(' ').toLowerCase();
}

function CreateCard(): JSX.Element {
  const [step, setStep] = useState<CreateStep>('version');
  const [sourceId, setSourceId] = useState('vanilla');
  const [selectedSelectionId, setSelectedSelectionId] = useState<string | null>(null);
  const [channel, setChannel] = useState<Channel>('release');
  const [query, setQuery] = useState('');
  const [nameOverride, setNameOverride] = useState<string | null>(null);
  const [viewError, setViewError] = useState<string | null>(null);
  const [viewLoading, setViewLoading] = useState(false);
  const [backendView, setBackendView] = useState<CreateBackendViewResponse | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const versionWellRef = useRef<HTMLDivElement | null>(null);
  const loadRequestRef = useRef(0);
  const versionListKey = `${sourceId}:${channel}:${query.trim().toLowerCase()}`;

  const totalGB = systemInfo.value?.total_memory_mb ? Math.floor(systemInfo.value.total_memory_mb / 1024) : 16;
  const memoryRec = getMemoryRecommendation(totalGB);
  const [memoryGB, setMemoryGB] = useState<number>(memoryRec.rec);
  const [paletteSeedOverride, setPaletteSeedOverride] = useState<number | null>(null);
  const [swatchPick, setSwatchPick] = useState<number | null>(null);
  const [jvmPreset, setJvmPreset] = useState('');
  const [presetOptions, setPresetOptions] = useState<CreatePresetOption[]>([]);
  const [optimizeChoice, setOptimizeChoice] = useState<boolean | null>(null);
  const [loaderChoice, setLoaderChoice] = useState<string | null>(null);
  const [loaderBuilds, setLoaderBuilds] = useState<CreateLoaderBuildsResponse | null>(null);
  const [loaderBuildsError, setLoaderBuildsError] = useState<string | null>(null);
  const theme = useTheme();
  const sourceKey = loaderKeyFromSourceId(sourceId);

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
  const selectablePresetOptions = useMemo(
    () => presetOptions.filter((option) => !option.disabled_reason),
    [presetOptions],
  );
  useEffect(() => {
    if (!windowPresets.some((p) => p.id === windowPresetId)) {
      setWindowPresetId('default');
    }
  }, [windowPresets, windowPresetId]);

  const cycleWindowPreset = (): void => {
    setWindowPresetId(nextWindowPreset(windowPresets, windowPresetId).id);
  };
  const cycleJvmPreset = (): void => {
    if (selectablePresetOptions.length === 0) return;
    const i = selectablePresetOptions.findIndex((option) => option.id === jvmPreset);
    const next =
      selectablePresetOptions[(i + 1) % selectablePresetOptions.length] ??
      selectablePresetOptions.find((option) => option.default);
    if (next) setJvmPreset(next.id);
  };

  const loadCreateView = async (source = sourceId): Promise<void> => {
    const requestId = loadRequestRef.current + 1;
    loadRequestRef.current = requestId;
    setViewLoading(true);
    setViewError(null);
    try {
      const res = (await api(
        'GET',
        `/instances/create-view?source=${encodeURIComponent(source)}`,
      )) as CreateBackendViewResponse & { error?: string };
      if (requestId !== loadRequestRef.current) return;
      if (res.error) throw new Error(res.error);
      setBackendView(res);
      const options = Array.isArray(res.preset_options)
        ? res.preset_options.filter(
            (option): option is CreatePresetOption =>
              typeof option.id === 'string' && typeof option.label === 'string' && typeof option.detail === 'string',
          )
        : [];
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
      const defaultSource = res.defaults?.source_id;
      const sources = Array.isArray(res.sources) ? res.sources.filter((option) => option.enabled) : [];
      const nextSource = sources.find((option) => option.id === defaultSource) ?? sources[0];
      if (nextSource)
        setSourceId((current) => (sources.some((option) => option.id === current) ? current : nextSource.id));
      const defaultChannel = normalizeChannel(res.defaults?.channel_id);
      setChannel((current) => (CHANNEL_ORDER.includes(current) ? current : defaultChannel));
    } catch (err: unknown) {
      if (requestId !== loadRequestRef.current) return;
      setBackendView(null);
      setPresetOptions([]);
      setViewError(errMessage(err));
    } finally {
      if (requestId === loadRequestRef.current) setViewLoading(false);
    }
  };

  useEffect(() => {
    void loadCreateView(sourceId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sourceId]);

  const searchInputRef = useRef<HTMLInputElement | null>(null);

  useLayoutEffect(() => {
    const node = versionWellRef.current;
    if (!node) return;
    node.scrollTop = 0;
  }, [versionListKey]);

  const sourceOptions = useMemo<CreateOption[]>(() => {
    return Array.isArray(backendView?.sources)
      ? backendView.sources.filter((option): option is CreateOption => {
          return typeof option.id === 'string' && typeof option.label === 'string';
        })
      : [];
  }, [backendView]);

  const channelOptions = useMemo<CreateOption[]>(() => {
    return Array.isArray(backendView?.channels)
      ? backendView.channels.filter((option): option is CreateOption => {
          return typeof option.id === 'string' && typeof option.label === 'string';
        })
      : [];
  }, [backendView]);

  useEffect(() => {
    if (sourceOptions.some((option) => option.id === sourceId)) return;
    const fallback = sourceOptions.find((option) => option.enabled);
    if (fallback) setSourceId(fallback.id);
  }, [sourceOptions, sourceId]);

  const backendRows = useMemo<CreateVersionRow[]>(() => {
    return Array.isArray(backendView?.versions)
      ? backendView.versions.filter((row): row is CreateVersionRow => {
          return (
            typeof row.source_id === 'string' &&
            typeof row.selection_id === 'string' &&
            typeof row.minecraft_version_id === 'string' &&
            typeof row.display_name === 'string'
          );
        })
      : [];
  }, [backendView]);

  const createNotices = useMemo<CreateNotice[]>(() => {
    return Array.isArray(backendView?.notices)
      ? backendView.notices.filter((notice): notice is CreateNotice => {
          return (
            typeof notice.state_id === 'string' && typeof notice.tone === 'string' && typeof notice.message === 'string'
          );
        })
      : [];
  }, [backendView]);

  const availableForSource = useMemo(
    () => backendRows.filter((row) => row.source_id === sourceId),
    [backendRows, sourceId],
  );

  const availableChannels = useMemo<Channel[]>(() => {
    const has: Record<Channel, boolean> = { release: false, snapshot: false, legacy: false, unknown: false };
    for (const row of availableForSource) has[normalizeChannel(row.channel)] = true;
    return CHANNEL_ORDER.filter((c) => has[c]);
  }, [availableForSource]);

  useEffect(() => {
    if (availableChannels.length === 0) return;
    if (availableChannels.includes(channel)) return;
    setChannel(availableChannels[0]!);
  }, [availableChannels, channel]);

  const versionRows: VersionRowModel[] = useMemo(() => {
    const q = query.trim().toLowerCase();
    return availableForSource
      .map((row) => ({
        id: row.minecraft_version_id,
        selectionId: row.selection_id,
        displayName: row.display_name,
        hint: row.hint ?? null,
        channel: normalizeChannel(row.channel),
        tags: normalizeVersionTags(row.tags),
        downloadState: normalizeDownloadState(row.download_state),
        createEnabled: row.create_enabled,
        disabledReason: row.disabled_reason ?? null,
      }))
      .filter((row) => row.channel === channel)
      .filter((row) => !q || rowSearchText(row).includes(q));
  }, [channel, query, availableForSource]);
  const selectedVersionRow = selectedSelectionId
    ? (versionRows.find((row) => row.selectionId === selectedSelectionId) ?? null)
    : null;
  const mcVersionId = selectedVersionRow?.id ?? null;

  const selectionId = selectedVersionRow?.selectionId ?? '';

  const suggestedName = useMemo(() => {
    if (!mcVersionId) return '';
    return defaultNameFor(sourceKey, mcVersionId);
  }, [sourceKey, mcVersionId]);

  const name = nameOverride ?? suggestedName;
  const displayName = name.trim() || suggestedName || 'New instance';

  const baseSeed = useMemo(() => {
    const previewId = `preview:${sourceId}:${mcVersionId ?? 'none'}`;
    return hashStr(`${previewId}:${displayName}:${mcVersionId ?? 'preview'}`) || 1;
  }, [sourceId, mcVersionId, displayName]);
  const paletteSeed = paletteSeedOverride ?? baseSeed;
  const swatchHues = useMemo(() => harmoniousTilePalette(theme, paletteSeed), [theme, paletteSeed]);
  const swatchIndex =
    swatchPick != null && swatchPick < swatchHues.length ? swatchPick : paletteSeed % swatchHues.length;
  const previewHue = swatchHues[swatchIndex] ?? theme.hue + 180;
  const previewSeed = seedForTileHue(previewHue);

  const pickSwatch = (index: number): void => {
    Sound.ui('click');
    setSwatchPick(index);
  };

  const previewInstance = {
    id: `preview:${sourceId}:${mcVersionId ?? 'none'}`,
    name: displayName,
    version_id: mcVersionId ?? '',
    art_seed: previewSeed,
  };

  const shufflePalette = (): void => {
    Sound.ui('click');
    setPaletteSeedOverride(nextArtSeed(paletteSeed));
  };

  useEffect(() => {
    if (!selectedSelectionId) return;
    if (versionRows.some((row) => row.selectionId === selectedSelectionId)) return;
    setSelectedSelectionId(null);
  }, [versionRows, selectedSelectionId]);

  const versionReady = Boolean(selectionId && selectedVersionRow?.createEnabled !== false);
  const createSelectionId = sourceKey === 'vanilla' ? selectionId : (loaderChoice ?? selectionId);
  const canCreate = versionReady && name.trim().length > 0 && !submitting;

  useEffect(() => {
    if (step === 'details' && !versionReady) setStep('version');
  }, [step, versionReady]);

  useEffect(() => {
    setLoaderBuilds(null);
    setLoaderBuildsError(null);
    setLoaderChoice(null);
    if (sourceKey === 'vanilla' || !mcVersionId) return;
    let cancelled = false;
    api(
      'GET',
      `/instances/create-view/loader-builds?source=${encodeURIComponent(sourceId)}&minecraft_version=${encodeURIComponent(mcVersionId)}`,
    )
      .then((res: any) => {
        if (cancelled) return;
        if (res.error) throw new Error(res.error);
        setLoaderBuilds(res as CreateLoaderBuildsResponse);
      })
      .catch((err: unknown) => {
        if (!cancelled) setLoaderBuildsError(errMessage(err));
      });
    return () => {
      cancelled = true;
    };
  }, [sourceKey, sourceId, mcVersionId]);

  const continueToDetails = (): void => {
    if (!versionReady) return;
    setStep('details');
  };

  const optimizeOption = backendView?.optimize_option ?? null;
  const autoOptimize = optimizeChoice ?? optimizeOption?.default_enabled ?? false;

  const submit = async (): Promise<void> => {
    if (submitting || !canCreate) return;
    const trimmed = name.trim();
    if (!trimmed || !selectionId) return;
    setSubmitting(true);
    try {
      const accentLabel = config.value?.theme ?? '';
      const winSpec = windowPresets.find((p) => p.id === windowPresetId);
      const dims = winSpec && winSpec.id !== 'default' ? { w: winSpec.w, h: winSpec.h } : null;
      const result = await createInstance({
        name: trimmed,
        selectionId: createSelectionId,
        icon: defaultIconFor(sourceKey),
        accent: accentLabel,
        initialSettings: {
          max_memory_mb: Math.round(memoryGB * 1024),
          art_seed: previewSeed,
          ...(dims ? { window_width: dims.w, window_height: dims.h } : {}),
          jvm_preset_id: jvmPreset,
          ...(optimizeOption ? { auto_optimize: autoOptimize } : {}),
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
      const inField = target != null && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA');

      if (e.key === 'Enter' && (e.ctrlKey || (!inField && target?.tagName !== 'BUTTON'))) {
        if (step === 'version') {
          if (versionReady) {
            e.preventDefault();
            continueToDetails();
          }
          return;
        }
        if (canCreate) {
          e.preventDefault();
          void submit();
        }
        return;
      }
      if (e.key === '/' && !inField) {
        if (step !== 'version') return;
        e.preventDefault();
        searchInputRef.current?.focus();
      }
    };
    window.addEventListener('keydown', handler);
    return () => {
      window.removeEventListener('keydown', handler);
    };
  }, [
    canCreate,
    submitting,
    sourceKey,
    selectedSelectionId,
    name,
    memoryGB,
    previewSeed,
    windowPresets,
    windowPresetId,
    jvmPreset,
    selectionId,
    createSelectionId,
    loaderChoice,
    autoOptimize,
    step,
    versionReady,
  ]);

  const availableChannelSet = new Set(availableChannels);
  const backendChannelTabs = channelOptions.map((option) => normalizeChannel(option.id));
  const channelTabs: Channel[] = (
    backendChannelTabs.length > 0
      ? [...backendChannelTabs, ...availableChannels.filter((c) => !backendChannelTabs.includes(c))]
      : availableChannels
  ).filter((value, index, values) => values.indexOf(value) === index);

  const winSpec = windowPresets.find((p) => p.id === windowPresetId) ??
    windowPresets[windowPresets.length - 1] ?? { id: 'default', label: 'Default', w: 0, h: 0 };
  const winSubtitle = winSpec.id === 'default' ? 'Game default' : `${winSpec.w} × ${winSpec.h}`;
  const selectedPresetOption =
    presetOptions.find((option) => option.id === jvmPreset) ?? presetOptions.find((option) => option.default) ?? null;
  const currentSourceLabel = sourceOptions.find((option) => option.id === sourceId)?.label ?? LOADER_LABELS[sourceKey];
  const loaderSelection = loaderChoice ?? loaderBuilds?.auto.selection_id ?? '';
  const loaderOptions = useMemo(() => {
    if (!loaderBuilds) return [];
    return [
      { value: loaderBuilds.auto.selection_id, label: loaderBuilds.auto.label },
      ...loaderBuilds.builds.map((build) => ({
        value: build.selection_id,
        label: [
          build.label,
          build.channel_label,
          build.recommended ? 'Recommended' : null,
          build.installed ? 'Installed' : null,
        ]
          .filter(Boolean)
          .join(' · '),
        disabled: !build.enabled,
      })),
    ];
  }, [loaderBuilds]);

  return (
    <>
      <header class="cp-cr-card-head">
        <div>
          <h1>Create instance</h1>
        </div>
        <IconButton icon="x" tooltip="Close (Esc)" onClick={closeCreate} />
        <div
          class="cp-cr-progress"
          data-step={step}
          role="status"
          aria-label={`Create step: ${step === 'version' ? 'Version' : 'Details'}`}
        >
          <span data-active={step === 'version'}>Version</span>
          <i aria-hidden="true" />
          <span data-active={step === 'details'}>Details</span>
        </div>
      </header>

      <div class="cp-cr-card-body" data-step={step}>
        {step === 'version' ? (
          <section class="cp-cr-pick" aria-label="Version">
            <div class="cp-seg cp-cr-sources" role="radiogroup" aria-label="Instance source">
              {sourceOptions.map((option) => {
                const key = loaderKeyFromSourceId(option.id);
                return (
                  <button
                    key={option.id}
                    type="button"
                    class="cp-cr-source"
                    data-active={sourceId === option.id}
                    role="radio"
                    aria-checked={sourceId === option.id}
                    title={option.disabled_reason ?? LOADER_TAGLINES[key]}
                    disabled={!option.enabled}
                    onClick={() => {
                      if (!option.enabled) return;
                      setSourceId(option.id);
                      setSelectedSelectionId(null);
                    }}
                  >
                    <LoaderLogo loader={key} size={14} class="cp-cr-loader-mark" />
                    <span>{option.label || LOADER_LABELS[key]}</span>
                  </button>
                );
              })}
            </div>

            {createNotices.length > 0 && (
              <div class="cp-cr-notices" aria-live="polite">
                {createNotices.map((notice) => {
                  const tone = noticeTone(notice.tone);
                  return (
                    <section
                      key={notice.state_id}
                      class="cp-notice"
                      data-tone={tone}
                      role={tone === 'warned' || tone === 'error' ? 'alert' : 'status'}
                    >
                      <span class="cp-notice-mark" aria-hidden="true">
                        <Icon name={noticeIcon(tone)} size={15} stroke={2.2} />
                      </span>
                      <div class="cp-notice-copy">
                        <strong>{notice.message}</strong>
                        {notice.detail && <p>{notice.detail}</p>}
                      </div>
                    </section>
                  );
                })}
              </div>
            )}

            <div class="cp-cr-vbar">
              <Input
                value={query}
                onChange={setQuery}
                placeholder="Filter versions…"
                icon="search"
                inputRef={searchInputRef}
                style={{ flex: 1 }}
              />
              <Segmented<Channel>
                role="tablist"
                size="sm"
                class="cp-cr-channels"
                ariaLabel="Release channel"
                value={channel}
                onChange={setChannel}
                options={channelTabs.map((value) => {
                  const available = availableChannelSet.has(value);
                  return {
                    value,
                    label: CHANNEL_LABEL[value],
                    disabled: !available,
                    ...(available ? {} : { title: `No ${CHANNEL_LABEL[value].toLowerCase()} versions here` }),
                  };
                })}
              />
            </div>

            <div class="cp-cr-vwell" ref={versionWellRef}>
              {viewLoading && (
                <div class="cp-cr-state">
                  <span class="cp-cr-spinner" aria-hidden="true" />
                  <span>Loading versions…</span>
                </div>
              )}
              {!viewLoading && viewError && (
                <div class="cp-cr-state is-error" role="alert" aria-live="polite">
                  <span>Couldn't load create options: {viewError}</span>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => {
                      void loadCreateView();
                    }}
                  >
                    Retry
                  </Button>
                </div>
              )}
              {!viewLoading && !viewError && versionRows.length === 0 && (
                <div class="cp-cr-state is-empty">
                  <span>Nothing matches.</span>
                </div>
              )}
              {versionRows.length > 0 && (
                <ul class="cp-cr-vlist" role="listbox" aria-label="Minecraft versions">
                  {versionRows.map((row) => (
                    <li
                      key={row.selectionId}
                      class="cp-cr-vrow"
                      data-active={selectedSelectionId === row.selectionId}
                      data-disabled={!row.createEnabled}
                      role="option"
                      aria-selected={selectedSelectionId === row.selectionId}
                      aria-disabled={!row.createEnabled}
                      title={row.disabledReason ?? undefined}
                      onClick={() => {
                        if (row.createEnabled) setSelectedSelectionId(row.selectionId);
                      }}
                    >
                      <span class="cp-cr-vrow-name">{row.displayName}</span>
                      {row.hint && <span class="cp-cr-vrow-hint">{row.hint}</span>}
                      {row.tags.map((tag) => (
                        <span key={tag.id} class="cp-cr-vrow-tag" title={tag.label}>
                          {tag.label}
                        </span>
                      ))}
                      <span class="cp-cr-vrow-spacer" />
                      {row.downloadState !== 'none' && (
                        <span
                          class="cp-cr-vrow-installed"
                          data-state={row.downloadState}
                          title={versionStatusTitle(row.downloadState, sourceKey)}
                        >
                          <Icon
                            name={row.downloadState === 'base' ? 'circle-dashed' : 'download'}
                            size={13}
                            stroke={2}
                          />
                        </span>
                      )}
                      {selectedSelectionId === row.selectionId && (
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
              <span>
                {versionRows.length} version{versionRows.length === 1 ? '' : 's'}
              </span>
              {sourceKey !== 'vanilla' && mcVersionId != null && (
                <div class="cp-cr-loader-pick">
                  <span class="cp-cr-loader-pick-label">Loader</span>
                  {loaderBuilds ? (
                    <SelectField
                      value={loaderSelection}
                      onChange={(next) => {
                        setLoaderChoice(next === loaderBuilds.auto.selection_id ? null : next);
                      }}
                      options={loaderOptions}
                      ariaLabel="Loader version"
                      width={230}
                    />
                  ) : loaderBuildsError ? (
                    <span class="cp-cr-hint cp-cr-hint--err">{loaderBuildsError}</span>
                  ) : (
                    <span class="cp-cr-hint">Loading…</span>
                  )}
                </div>
              )}
            </div>
          </section>
        ) : (
          <section class="cp-cr-details" aria-label="Instance details" style={{ ['--cp-tile-h' as any]: previewHue }}>
            <div class="cp-cr-identity">
              <div class="cp-cr-tilebox">
                <InstanceTile inst={previewInstance} radius={16} />
              </div>
              <div class="cp-cr-id-col">
                <div class="cp-cr-kicker">
                  <Pill>{currentSourceLabel}</Pill>
                  <span class="cp-cr-kicker-mc">
                    {selectedVersionRow ? `Minecraft ${selectedVersionRow.displayName}` : 'No version yet'}
                  </span>
                </div>
                <Input
                  value={name}
                  onChange={(v) => setNameOverride(v)}
                  placeholder={suggestedName || 'New instance'}
                />
              </div>
            </div>

            <div class="cp-cr-color" role="group" aria-label="Instance color">
              <span class="cp-cr-field-label" id="cp-cr-color-label">
                Color
              </span>
              <div class="cp-cr-swatches" aria-labelledby="cp-cr-color-label">
                {swatchHues.map((hue, index) => (
                  <button
                    key={`${paletteSeed}:${index}`}
                    type="button"
                    class="cp-cr-swatch"
                    data-active={index === swatchIndex}
                    style={{ ['--sw' as any]: hue }}
                    aria-label={`Use color ${index + 1} of ${swatchHues.length}`}
                    aria-pressed={index === swatchIndex}
                    onClick={() => pickSwatch(index)}
                  />
                ))}
                <button
                  type="button"
                  class="cp-cr-shuffle"
                  title="Shuffle colors"
                  aria-label="Shuffle colors"
                  onClick={shufflePalette}
                >
                  <Icon name="refresh" size={13} stroke={2} />
                </button>
              </div>
            </div>

            <div class="cp-cr-fields">
              <div class="cp-cr-field">
                <div class="cp-cr-mem-head">
                  <span class="cp-cr-field-label">Memory</span>
                  <span class="cp-cr-mem-reading" aria-live="polite">
                    {fmtMem(memoryGB)}
                  </span>
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
                  title={selectedPresetOption?.disabled_reason ?? selectedPresetOption?.detail ?? undefined}
                  disabled={selectablePresetOptions.length === 0}
                >
                  <span class="cp-cr-row-key">Profile</span>
                  <span class="cp-cr-row-val">
                    <span class="cp-cr-row-value">{selectedPresetOption?.label ?? 'Loading'}</span>
                    <Icon name="chevron-right" size={13} stroke={2} />
                  </span>
                </button>
                {optimizeOption && (
                  <div class="cp-cr-row cp-cr-row--switch">
                    <span class="cp-cr-row-key">
                      {optimizeOption.label}
                      <span class="cp-cr-row-detail">{optimizeOption.detail}</span>
                    </span>
                    <Toggle
                      on={autoOptimize}
                      onChange={() => {
                        Sound.ui('click');
                        setOptimizeChoice(!autoOptimize);
                      }}
                    />
                  </div>
                )}
              </div>
            </div>
          </section>
        )}
      </div>

      <footer class="cp-cr-card-foot">
        <span class="cp-cr-footnote" aria-live="polite">
          {step === 'version'
            ? !versionReady
              ? 'Pick a Minecraft version to continue.'
              : 'Continue to name and settings.'
            : !versionReady
              ? 'Pick a Minecraft version to continue.'
              : canCreate
                ? 'Enter creates the instance.'
                : 'Name your instance to create it.'}
        </span>
        <div class="cp-cr-foot-actions">
          {step === 'version' ? (
            <>
              <Button variant="ghost" onClick={closeCreate} disabled={submitting}>
                Cancel
              </Button>
              <Button
                trailing={<Icon name="arrow-right" size={15} stroke={1.8} />}
                onClick={continueToDetails}
                disabled={!versionReady || submitting}
              >
                Continue
              </Button>
            </>
          ) : (
            <>
              <Button variant="ghost" icon="arrow-left" onClick={() => setStep('version')} disabled={submitting}>
                Back
              </Button>
              <Button
                icon="plus"
                onClick={() => {
                  void submit();
                }}
                disabled={!canCreate}
                sound="affirm"
              >
                {submitting ? 'Creating…' : 'Create instance'}
              </Button>
            </>
          )}
        </div>
      </footer>
    </>
  );
}
