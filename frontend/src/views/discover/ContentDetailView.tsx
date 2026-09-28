import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { Button, IconButton, Pill } from '../../ui/Atoms';
import { Icon, type IconName } from '../../ui/Icons';
import { Modal, ModalContent } from '../../ui/Modal';
import { SelectField, type SelectFieldOption } from '../../ui/Select';
import { navigate, route } from '../../ui-state';
import { cachedDetail, loadDetail } from './detail-cache';
import { formatAge, formatBytes, formatCount, formatDate, plural } from '../../format';
import { errMessage } from '../../utils';
import type { ContentDetail, ContentVersion, GalleryImage, ReleaseChannel } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { setUpModpack } from './actions';
import { Tray } from './Tray';
import { TargetBar } from './TargetBar';
import { InstallConflictSheet, useInstallFlow, type InstallFlow } from './install-flow';
import { ModpackPicker } from './ModpackPicker';
import { isStaged, stageContent, stagedItem, targetInstance, unstage } from './state';
import { ProjectBody, ExternalLink } from './markdown';
import {
  compareMcDesc,
  ContentIcon,
  InstanceTargetPicker,
  isAddable,
  KIND_NOUN,
  Spinner,
  tagLabel,
  versionFits,
} from './shared';

type Tab = 'about' | 'gallery' | 'versions';

interface DetailViewState {
  canonicalId: string;
  detail: ContentDetail | null;
  error: string | null;
}

export function ContentDetailView(): JSX.Element {
  const current = route.value;
  const canonicalId = current.name === 'content' ? current.id : '';
  const instance = targetInstance.value;

  const [detailState, setDetailState] = useState<DetailViewState>(() => ({
    canonicalId,
    detail: cachedDetail(canonicalId) ?? null,
    error: null,
  }));
  const [tab, setTab] = useState<Tab>('about');
  const flow = useInstallFlow(instance?.id);
  const currentDetailState = detailState.canonicalId === canonicalId ? detailState : null;
  const detail = currentDetailState?.detail ?? null;
  const error = currentDetailState?.error ?? null;

  useEffect(() => {
    const cached = cachedDetail(canonicalId);
    let active = true;
    setDetailState({ canonicalId, detail: cached ?? null, error: null });
    setTab('about');
    loadDetail(canonicalId)
      .then((resolved) => {
        if (active) setDetailState({ canonicalId, detail: resolved, error: null });
      })
      .catch((err) => {
        if (active && !cached) setDetailState({ canonicalId, detail: null, error: errMessage(err) });
      });
    return () => {
      active = false;
    };
  }, [canonicalId]);

  const back = (): void => navigate({ name: 'discover', target: instance?.id });

  if (error) {
    return (
      <div class="cp-view-page">
        <BackLink onClick={back} />
        <div class="cp-resource-empty">
          <span>
            <Icon name="alert" size={20} />
          </span>
          <strong>Could not load this page</strong>
          <p>{error}</p>
        </div>
        <Tray />
      </div>
    );
  }

  if (!detail) {
    return (
      <div class="cp-view-page">
        <BackLink onClick={back} />
        <div class="cp-content-hero">
          <div class="cp-content-hero-icon cp-skeleton" />
          <div class="cp-content-hero-id">
            <div class="cp-skeleton cp-skeleton-line" style={{ width: 120 }} />
            <div class="cp-skeleton cp-skeleton-line" style={{ width: 280, height: 30, marginTop: 12 }} />
            <div class="cp-skeleton cp-skeleton-line" style={{ width: 360, marginTop: 14 }} />
          </div>
        </div>
        <Tray />
      </div>
    );
  }

  const gallery = detail.gallery ?? [];
  const versions = detail.versions ?? [];
  const latest = versions[0];
  const tabs: Array<{ id: Tab; icon: IconName; label: string; count?: number }> = [
    { id: 'about', icon: 'info', label: 'About' },
  ];
  if (gallery.length > 0) tabs.push({ id: 'gallery', icon: 'image', label: 'Gallery', count: gallery.length });
  if (versions.length > 0) tabs.push({ id: 'versions', icon: 'archive', label: 'Versions', count: versions.length });
  const activeTab = tabs.some((entry) => entry.id === tab) ? tab : 'about';

  return (
    <div class="cp-view-page">
      <div class="cp-content-back-row">
        <BackLink onClick={back} />
        {instance && <TargetBar instance={instance} />}
      </div>

      <header class="cp-content-hero">
        <div class="cp-content-hero-icon">
          <ContentIcon url={detail.icon_url} kind={detail.kind} size={40} />
        </div>
        <div class="cp-content-hero-id">
          <div class="cp-content-hero-kicker">
            <Pill icon={detail.kind === 'modpack' ? 'stack' : 'tag'}>{KIND_NOUN[detail.kind]}</Pill>
            {detail.categories.slice(0, 3).map((category) => (
              <span key={category} class="cp-content-hero-cat">
                {tagLabel(category)}
              </span>
            ))}
          </div>
          <h1 class="cp-content-hero-title">{detail.title}</h1>
          <div class="cp-content-hero-meta">
            {detail.author && (
              <span class="cp-content-hero-by">
                by <b>{detail.author}</b>
              </span>
            )}
            <span class="cp-content-stat" title={`${detail.downloads.toLocaleString()} downloads`}>
              <Icon name="download" size={13} />
              <b>{formatCount(detail.downloads)}</b>
            </span>
            <span class="cp-content-stat" title={`${detail.follows.toLocaleString()} followers`}>
              <Icon name="user" size={13} />
              <b>{formatCount(detail.follows)}</b>
            </span>
            {detail.updated && (
              <span class="cp-content-stat" title={`Updated ${formatAge(detail.updated)}`}>
                <Icon name="clock" size={13} />
                {formatAge(detail.updated)}
              </span>
            )}
            {latest && (
              <span class="cp-content-stat" title={`Latest version ${latest.version_number}`}>
                <Icon name="tag" size={13} />
                {latest.version_number}
              </span>
            )}
          </div>
        </div>
        <div class="cp-content-hero-actions">
          <InstallAction detail={detail} instance={instance} flow={flow} />
          {detail.slug && (
            <ExternalLink href={`https://modrinth.com/project/${detail.slug}`} class="cp-content-out">
              <IconButton icon="expand" tooltip="Open on Modrinth" />
            </ExternalLink>
          )}
        </div>
      </header>

      {tabs.length > 1 && (
        <div class="cp-tabs" role="tablist">
          {tabs.map((entry) => (
            <button
              key={entry.id}
              type="button"
              role="tab"
              aria-selected={entry.id === activeTab}
              data-active={entry.id === activeTab}
              onClick={() => setTab(entry.id)}
            >
              <span class="cp-tab-icon">
                <Icon name={entry.icon} size={15} />
              </span>
              <span class="cp-tab-label">{entry.label}</span>
              {entry.count != null && <span class="cp-tab-count">{entry.count}</span>}
            </button>
          ))}
        </div>
      )}

      {activeTab === 'about' && <AboutPane detail={detail} />}
      {activeTab === 'gallery' && <GalleryPane detail={detail} />}
      {activeTab === 'versions' && <VersionsPane detail={detail} instance={instance} flow={flow} />}

      <InstallConflictSheet flow={flow} />
      <Tray />
    </div>
  );
}

function BackLink({ onClick }: { onClick: () => void }): JSX.Element {
  return (
    <div class="cp-content-back">
      <Button variant="ghost" size="sm" icon="chevron-left" onClick={onClick}>
        Discover
      </Button>
    </div>
  );
}

function InstallAction({
  detail,
  instance,
  flow,
}: {
  detail: ContentDetail;
  instance: EnrichedInstance | null;
  flow: InstallFlow;
}): JSX.Element | null {
  const [busy, setBusy] = useState(false);
  const [pickingPack, setPickingPack] = useState(false);
  const staged = isStaged(detail.canonical_id);
  const versions = detail.versions ?? [];
  const fits = versions.length === 0 || versions.some((version) => versionFits(version, detail.kind, instance));

  if (detail.kind === 'modpack') {
    if (instance) {
      return (
        <>
          <Button size="lg" icon="plus" onClick={() => setPickingPack(true)}>
            Choose files
          </Button>
          <ModpackPicker
            open={pickingPack}
            instanceId={instance.id}
            canonicalId={detail.canonical_id}
            onClose={() => setPickingPack(false)}
          />
        </>
      );
    }
    return (
      <Button
        size="lg"
        icon="stack"
        disabled={busy}
        onClick={() => {
          setBusy(true);
          void setUpModpack(detail.canonical_id, undefined, detail.icon_url).finally(() => setBusy(false));
        }}
      >
        {busy ? 'Setting up…' : 'Set up an instance'}
      </Button>
    );
  }

  if (!isAddable(detail.kind)) return null;

  if (instance) {
    return (
      <>
        {!fits && (
          <span class="cp-content-blocked" title={`No version supports ${instance.version_display.summary_label}`}>
            <Icon name="alert" size={13} />
            No version fits
          </span>
        )}
        <Button
          size="lg"
          icon="download"
          disabled={flow.busy || !fits}
          onClick={() => void flow.add([{ canonical_id: detail.canonical_id, kind: detail.kind }], detail.title)}
        >
          {flow.busy ? 'Adding…' : `Add to ${instance.name}`}
        </Button>
      </>
    );
  }

  return (
    <>
      <InstanceTargetPicker
        placeholder="Add to…"
        width={170}
        onPick={(id) => navigate({ name: 'content', id: detail.canonical_id, target: id })}
      />
      <Button
        size="lg"
        variant={staged ? 'secondary' : 'primary'}
        icon={staged ? 'check' : 'plus'}
        onClick={() => {
          if (staged) unstage(detail.canonical_id);
          else stageContent(detail);
        }}
      >
        {staged ? 'Staged' : 'Stage'}
      </Button>
    </>
  );
}

function AboutPane({ detail }: { detail: ContentDetail }): JSX.Element {
  const body = detail.body?.trim();
  return <div class="cp-content-pane">{body ? <ProjectBody body={body} /> : <p>{detail.summary}</p>}</div>;
}

function GalleryShot({
  image,
  index,
  onOpen,
}: {
  image: GalleryImage;
  index: number;
  onOpen: () => void;
}): JSX.Element {
  const [loaded, setLoaded] = useState(false);

  return (
    <button
      type="button"
      class="cp-content-shot"
      data-loaded={loaded}
      onClick={onOpen}
      aria-label={image.title ?? `Screenshot ${index + 1}`}
      title={image.title}
    >
      <img
        src={image.url}
        alt={image.title ?? ''}
        loading="lazy"
        decoding="async"
        onLoad={() => setLoaded(true)}
        onError={() => setLoaded(true)}
      />
      {image.title && <span class="cp-content-shot-cap">{image.title}</span>}
    </button>
  );
}

function GalleryPane({ detail }: { detail: ContentDetail }): JSX.Element {
  const gallery = detail.gallery ?? [];
  const [open, setOpen] = useState<number | null>(null);
  const shot = open != null ? gallery[open] : null;

  return (
    <>
      <div class="cp-content-gallery">
        {gallery.map((image, index) => (
          <GalleryShot key={image.url} image={image} index={index} onOpen={() => setOpen(index)} />
        ))}
      </div>

      {shot && (
        <Modal open onOpenChange={(next) => !next && setOpen(null)}>
          <ModalContent className="cp-content-lightbox" aria-label={shot.title ?? 'Screenshot'}>
            <img src={shot.url} alt={shot.title ?? ''} />
            {shot.title && <div class="cp-content-lightbox-cap">{shot.title}</div>}
          </ModalContent>
        </Modal>
      )}
    </>
  );
}

const CHANNEL_FILTERS: Array<{ value: '' | ReleaseChannel; label: string }> = [
  { value: '', label: 'All channels' },
  { value: 'release', label: 'Release' },
  { value: 'beta', label: 'Beta' },
  { value: 'alpha', label: 'Alpha' },
];

function VersionRowAction({
  detail,
  version,
  instance,
  busy,
  onInstall,
}: {
  detail: ContentDetail;
  version: ContentVersion;
  instance: EnrichedInstance | null;
  busy: string;
  onInstall: (version: ContentVersion) => void;
}): JSX.Element | null {
  const [working, setWorking] = useState(false);
  const [pickingPack, setPickingPack] = useState(false);

  if (detail.kind === 'modpack') {
    if (instance) {
      return (
        <>
          <Button variant="secondary" size="sm" icon="plus" onClick={() => setPickingPack(true)}>
            Choose files
          </Button>
          <ModpackPicker
            open={pickingPack}
            instanceId={instance.id}
            canonicalId={detail.canonical_id}
            versionId={version.id}
            onClose={() => setPickingPack(false)}
          />
        </>
      );
    }
    return (
      <Button
        variant="secondary"
        size="sm"
        icon="stack"
        disabled={working}
        title={`Create an instance from ${version.version_number}`}
        onClick={() => {
          setWorking(true);
          void setUpModpack(detail.canonical_id, version.id, detail.icon_url).finally(() => setWorking(false));
        }}
      >
        {working ? <Spinner size={12} /> : 'Set up'}
      </Button>
    );
  }

  if (!isAddable(detail.kind)) return null;

  if (instance) {
    const fits = versionFits(version, detail.kind, instance);
    if (!fits) {
      return (
        <span class="cp-content-version-no" title={`Does not fit ${instance.version_display.summary_label}`}>
          Does not fit
        </span>
      );
    }
    return (
      <Button
        variant="secondary"
        size="sm"
        icon="plus"
        disabled={!!busy}
        title={`Add ${version.version_number} to ${instance.name}`}
        onClick={() => onInstall(version)}
      >
        {busy === version.id ? <Spinner size={12} /> : 'Add'}
      </Button>
    );
  }

  const stagedEntry = stagedItem(detail.canonical_id);
  const stagedThis = stagedEntry?.version_id === version.id;
  return (
    <Button
      variant={stagedThis ? 'primary' : 'secondary'}
      size="sm"
      icon={stagedThis ? 'check' : 'plus'}
      title={
        stagedThis
          ? 'Remove from selection'
          : stagedEntry
            ? `Switch the staged version to ${version.version_number}`
            : `Stage ${version.version_number}`
      }
      onClick={() => {
        if (stagedThis) unstage(detail.canonical_id);
        else stageContent(detail, version);
      }}
    >
      {stagedThis ? 'Staged' : 'Stage'}
    </Button>
  );
}

function VersionsPane({
  detail,
  instance,
  flow,
}: {
  detail: ContentDetail;
  instance: EnrichedInstance | null;
  flow: InstallFlow;
}): JSX.Element {
  const [busy, setBusy] = useState('');
  const [showAll, setShowAll] = useState(false);
  const [channel, setChannel] = useState<'' | ReleaseChannel>('');
  const [mcFilter, setMcFilter] = useState('');
  const [loaderFilter, setLoaderFilter] = useState('');
  const versions = detail.versions ?? [];

  const channelOptions = useMemo<SelectFieldOption<'' | ReleaseChannel>[]>(() => {
    const present = new Set(versions.map((version) => version.channel));
    return CHANNEL_FILTERS.filter((option) => option.value === '' || present.has(option.value));
  }, [versions]);

  const mcOptions = useMemo<SelectFieldOption<string>[]>(() => {
    const set = new Set<string>();
    for (const version of versions) for (const value of version.game_versions) set.add(value);
    const list = [...set].sort(compareMcDesc);
    return [{ value: '', label: 'Any version' }, ...list.map((value) => ({ value, label: value }))];
  }, [versions]);

  const loaderOptions = useMemo<SelectFieldOption<string>[]>(() => {
    const set = new Set<string>();
    for (const version of versions) for (const value of version.loaders) set.add(value);
    const list = [...set].sort();
    return [{ value: '', label: 'Any loader' }, ...list.map((value) => ({ value, label: tagLabel(value) }))];
  }, [versions]);

  const filtered = useMemo(
    () =>
      versions.filter(
        (version) =>
          (!channel || version.channel === channel) &&
          (!mcFilter || version.game_versions.includes(mcFilter)) &&
          (!loaderFilter || version.loaders.includes(loaderFilter)),
      ),
    [versions, channel, mcFilter, loaderFilter],
  );
  const shown = showAll ? filtered : filtered.slice(0, 10);
  const isFiltered = Boolean(channel || mcFilter || loaderFilter);

  const install = async (version: ContentVersion): Promise<void> => {
    if (!instance || busy) return;
    setBusy(version.id);
    await flow.add(
      [{ canonical_id: detail.canonical_id, kind: detail.kind, version_id: version.id }],
      `${detail.title} ${version.version_number}`,
    );
    setBusy('');
  };

  return (
    <div class="cp-content-versions">
      <div class="cp-content-version-bar">
        {channelOptions.length > 2 && (
          <SelectField value={channel} onChange={setChannel} options={channelOptions} ariaLabel="Channel" width={130} />
        )}
        {mcOptions.length > 1 && (
          <SelectField
            value={mcFilter}
            onChange={setMcFilter}
            options={mcOptions}
            ariaLabel="Minecraft version"
            width={130}
          />
        )}
        {loaderOptions.length > 2 && (
          <SelectField
            value={loaderFilter}
            onChange={setLoaderFilter}
            options={loaderOptions}
            ariaLabel="Loader"
            width={130}
          />
        )}
        <span class="cp-content-version-bar-count" aria-live="polite">
          {plural(filtered.length, 'version', 'versions')}
        </span>
      </div>
      <div class="cp-content-version-head" aria-hidden="true">
        <span>Version</span>
        <span>Supports</span>
        <span>Published</span>
        <span>Downloads</span>
        <span>Size</span>
        <span />
      </div>
      {filtered.length === 0 && (
        <div class="cp-content-version-empty">
          <span>Nothing matches these filters.</span>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              setChannel('');
              setMcFilter('');
              setLoaderFilter('');
            }}
          >
            Clear filters
          </Button>
        </div>
      )}
      {shown.map((version) => {
        const fits = versionFits(version, detail.kind, instance);
        const file = version.files.find((entry) => entry.primary) ?? version.files[0];
        return (
          <div key={version.id} class="cp-content-version" data-fits={fits}>
            <span class="cp-content-version-id">
              <span class="cp-content-version-name" title={version.name}>
                {version.version_number}
              </span>
              {version.channel !== 'release' && (
                <Pill tone={version.channel === 'beta' ? 'warn' : 'err'}>{version.channel}</Pill>
              )}
            </span>
            <span class="cp-content-version-tags">
              {version.loaders.map((entry) => (
                <span key={entry}>{entry}</span>
              ))}
              {version.game_versions.slice(0, 3).map((entry) => (
                <span key={entry}>{entry}</span>
              ))}
              {version.game_versions.length > 3 && <span>+{version.game_versions.length - 3}</span>}
            </span>
            <span class="cp-content-version-cell">{formatDate(version.published)}</span>
            <span class="cp-content-version-cell">{formatCount(version.downloads)}</span>
            <span class="cp-content-version-cell">{file?.size ? formatBytes(file.size) : ''}</span>
            <span class="cp-content-version-act">
              <VersionRowAction
                detail={detail}
                version={version}
                instance={instance}
                busy={busy}
                onInstall={(entry) => void install(entry)}
              />
            </span>
          </div>
        );
      })}
      {filtered.length > shown.length && (
        <button type="button" class="cp-content-version-more" onClick={() => setShowAll(true)}>
          Show {plural(filtered.length - shown.length, 'older version', 'older versions')}
          {isFiltered ? ' matching these filters' : ''}
        </button>
      )}
    </div>
  );
}
