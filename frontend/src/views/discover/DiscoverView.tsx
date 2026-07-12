import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Input, Pill } from '../../ui/Atoms';
import { Segmented } from '../../ui/Segmented';
import { SelectField, type SelectFieldOption } from '../../ui/Select';
import { Modal, ModalContent } from '../../ui/Modal';
import { Icon } from '../../ui/Icons';
import { instances, versions } from '../../store';
import { toast } from '../../toast';
import { getContentDetail, installContent, planContent, searchContent } from '../../content';
import type { CanonicalContent, ContentDetail, ContentKind, ContentSort, ResolutionPlan } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';

const KIND_TABS: { value: ContentKind; label: string; icon: string }[] = [
  { value: 'mod', label: 'Mods', icon: 'puzzle' },
  { value: 'modpack', label: 'Modpacks', icon: 'archive' },
  { value: 'resource_pack', label: 'Resource packs', icon: 'image' },
  { value: 'shader_pack', label: 'Shaders', icon: 'sparkles' },
];

const SORT_OPTIONS: SelectFieldOption<ContentSort>[] = [
  { value: 'relevance', label: 'Relevance' },
  { value: 'downloads', label: 'Most downloads' },
  { value: 'follows', label: 'Most followed' },
  { value: 'updated', label: 'Recently updated' },
  { value: 'newest', label: 'Newest' },
];

const LOADER_OPTIONS: SelectFieldOption<string>[] = [
  { value: '', label: 'Any loader' },
  { value: 'fabric', label: 'Fabric' },
  { value: 'forge', label: 'Forge' },
  { value: 'neoforge', label: 'NeoForge' },
  { value: 'quilt', label: 'Quilt' },
];

const RECENT_MC = ['1.21.6', '1.21.5', '1.21.4', '1.21.3', '1.21.1', '1.21', '1.20.6', '1.20.4', '1.20.1'];
const PAGE_SIZE = 40;
const SEARCH_DEBOUNCE_MS = 220;

function usesLoaderFilter(kind: ContentKind): boolean {
  return kind === 'mod' || kind === 'modpack';
}

function compareMcDesc(a: string, b: string): number {
  const pa = a.split('.').map(Number);
  const pb = b.split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i += 1) {
    const diff = (pb[i] ?? 0) - (pa[i] ?? 0);
    if (diff !== 0) return diff;
  }
  return 0;
}

function formatCount(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(value >= 10_000_000 ? 0 : 1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(value >= 10_000 ? 0 : 1)}k`;
  return String(value);
}

function formatBytes(bytes: number): string {
  if (!bytes) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exponent;
  return `${value.toFixed(value >= 10 || exponent === 0 ? 0 : 1)} ${units[exponent]}`;
}

function Spinner({ size = 14 }: { size?: number }): JSX.Element {
  return <span class="cp-discover-spinner" style={{ width: size, height: size }} aria-hidden="true" />;
}

function ContentCard({
  item,
  onOpen,
}: {
  item: CanonicalContent;
  onOpen: (item: CanonicalContent) => void;
}): JSX.Element {
  return (
    <button class="cp-discover-card" onClick={() => onOpen(item)}>
      <div class="cp-discover-card-icon" aria-hidden="true">
        {item.icon_url ? <img src={item.icon_url} alt="" loading="lazy" /> : <Icon name="puzzle" size={22} />}
      </div>
      <div class="cp-discover-card-body">
        <div class="cp-discover-card-title" title={item.title}>
          {item.title}
        </div>
        {item.author && <div class="cp-discover-card-author">by {item.author}</div>}
        <p class="cp-discover-card-summary">{item.summary}</p>
        <div class="cp-discover-card-meta">
          <span title={`${item.downloads.toLocaleString()} downloads`}>
            <Icon name="download" size={12} /> {formatCount(item.downloads)}
          </span>
          {item.categories.slice(0, 2).map((category) => (
            <span key={category} class="cp-discover-tag">
              {category}
            </span>
          ))}
        </div>
      </div>
    </button>
  );
}

function SkeletonCard(): JSX.Element {
  return (
    <div class="cp-discover-card cp-discover-card--skeleton" aria-hidden="true">
      <div class="cp-discover-card-icon cp-skeleton" />
      <div class="cp-discover-card-body">
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '60%' }} />
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '35%' }} />
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '100%', marginTop: 8 }} />
        <div class="cp-skeleton cp-skeleton-line" style={{ width: '80%' }} />
      </div>
    </div>
  );
}

function InstallPanel({ item }: { item: CanonicalContent }): JSX.Element {
  const moddedInstances = useMemo(
    () => (instances.value as EnrichedInstance[]).filter((instance) => instance.version_display.supports_mods),
    [instances.value],
  );
  const instanceOptions = useMemo<SelectFieldOption<string>[]>(
    () =>
      moddedInstances.map((instance) => ({
        value: instance.id,
        label: `${instance.name} · ${instance.version_display.summary_label}`,
      })),
    [moddedInstances],
  );
  const [instanceId, setInstanceId] = useState<string>(() => moddedInstances[0]?.id ?? '');
  const [plan, setPlan] = useState<ResolutionPlan | null>(null);
  const [planning, setPlanning] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [installed, setInstalled] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const installable = item.kind === 'mod';

  useEffect(() => {
    if (!installable || !instanceId) {
      setPlan(null);
      return;
    }
    let active = true;
    setPlanning(true);
    setError(null);
    setInstalled(false);
    planContent(instanceId, [{ canonical_id: item.canonical_id, kind: item.kind }])
      .then((resolved) => {
        if (active) setPlan(resolved);
      })
      .catch((err) => {
        if (active) setError(err?.message || 'Could not build an install plan.');
      })
      .finally(() => {
        if (active) setPlanning(false);
      });
    return () => {
      active = false;
    };
  }, [instanceId, item.canonical_id, installable]);

  if (!installable) {
    const label = KIND_TABS.find((tab) => tab.value === item.kind)?.label.toLowerCase() ?? 'this content';
    return <div class="cp-discover-install cp-discover-install--muted">Installing {label} is coming soon.</div>;
  }

  if (moddedInstances.length === 0) {
    return (
      <div class="cp-discover-install cp-discover-install--muted">
        Create a modded instance (Fabric, Forge, NeoForge, or Quilt) to add mods.
      </div>
    );
  }

  const toInstall = plan?.items.filter((planItem) => !planItem.already_installed || planItem.update) ?? [];
  const dependencyCount = toInstall.filter((planItem) => planItem.reason === 'dependency').length;
  const nothingToDo = !!plan && toInstall.length === 0 && plan.conflicts.length === 0;

  const install = (): void => {
    if (!instanceId || installing) return;
    setInstalling(true);
    setError(null);
    installContent(instanceId, [{ canonical_id: item.canonical_id, kind: item.kind }])
      .then(() => {
        setInstalled(true);
        toast(`Added ${item.title}`, 'success');
      })
      .catch((err) => {
        setError(err?.message || 'Install failed.');
        toast(err?.message || 'Install failed', 'error');
      })
      .finally(() => setInstalling(false));
  };

  return (
    <div class="cp-discover-install">
      <label class="cp-discover-install-label" for="cp-discover-instance">
        Add to instance
      </label>
      <SelectField
        value={instanceId}
        onChange={setInstanceId}
        options={instanceOptions}
        ariaLabel="Choose an instance"
        width="100%"
      />

      {planning && (
        <div class="cp-discover-plan-note">
          <Spinner size={12} /> Checking compatibility…
        </div>
      )}

      {plan && !planning && (
        <div class="cp-discover-plan">
          {plan.conflicts.map((conflict, index) => (
            <div key={index} class="cp-discover-conflict">
              <Icon name="alert" size={13} /> {conflict.detail}
            </div>
          ))}
          {nothingToDo && <div class="cp-discover-plan-note">Already up to date in this instance.</div>}
          {toInstall.length > 0 && (
            <div class="cp-discover-plan-note">
              {toInstall.length} file{toInstall.length === 1 ? '' : 's'}
              {dependencyCount > 0 ? ` · ${dependencyCount} dependenc${dependencyCount === 1 ? 'y' : 'ies'}` : ''}
              {plan.total_download_bytes > 0 ? ` · ${formatBytes(plan.total_download_bytes)}` : ''}
            </div>
          )}
        </div>
      )}

      {error && <div class="cp-discover-conflict">{error}</div>}

      <Button
        icon={installed ? 'check' : 'download'}
        onClick={install}
        disabled={installing || planning || !instanceId}
        full
      >
        {installing ? 'Installing…' : installed ? 'Added' : nothingToDo ? 'Reinstall' : 'Install'}
      </Button>
    </div>
  );
}

function DetailModal({ item, onClose }: { item: CanonicalContent; onClose: () => void }): JSX.Element {
  const [detail, setDetail] = useState<ContentDetail | null>(null);

  useEffect(() => {
    let active = true;
    setDetail(null);
    getContentDetail(item.canonical_id)
      .then((resolved) => {
        if (active) setDetail(resolved);
      })
      .catch(() => {});
    return () => {
      active = false;
    };
  }, [item.canonical_id]);

  const gallery = detail?.gallery ?? [];

  return (
    <Modal open onOpenChange={(next) => !next && onClose()}>
      <ModalContent className="cp-discover-sheet" aria-label={item.title}>
        <div class="cp-discover-sheet-head">
          <div class="cp-discover-card-icon cp-discover-sheet-icon" aria-hidden="true">
            {item.icon_url ? <img src={item.icon_url} alt="" /> : <Icon name="puzzle" size={26} />}
          </div>
          <div style={{ flex: 1, minWidth: 0 }}>
            <h2 class="cp-discover-sheet-title" title={item.title}>
              {item.title}
            </h2>
            {item.author && <div class="cp-discover-card-author">by {item.author}</div>}
          </div>
        </div>

        <div class="cp-discover-sheet-meta">
          <Pill icon="download">{formatCount(item.downloads)}</Pill>
          {item.categories.slice(0, 4).map((category) => (
            <span key={category} class="cp-discover-tag">
              {category}
            </span>
          ))}
        </div>

        <p class="cp-discover-sheet-summary">{item.summary}</p>

        {gallery.length > 0 && (
          <div class="cp-discover-gallery">
            {gallery.slice(0, 6).map((image) => (
              <img key={image.url} src={image.url} alt={image.title ?? ''} loading="lazy" />
            ))}
          </div>
        )}

        <InstallPanel item={item} />

        {detail && detail.versions.length > 0 && (
          <div class="cp-discover-versions">
            <div class="cp-discover-versions-head">Recent versions</div>
            {detail.versions.slice(0, 5).map((version) => (
              <div key={version.id} class="cp-discover-version-row">
                <span class="cp-discover-version-name" title={version.name}>
                  {version.version_number}
                </span>
                <span class="cp-discover-version-loaders">{version.loaders.join(', ')}</span>
                <span class="cp-discover-version-channel" data-channel={version.channel}>
                  {version.channel}
                </span>
              </div>
            ))}
          </div>
        )}
      </ModalContent>
    </Modal>
  );
}

export function DiscoverView(): JSX.Element {
  const [kind, setKind] = useState<ContentKind>('mod');
  const [query, setQuery] = useState('');
  const [loader, setLoader] = useState('');
  const [gameVersion, setGameVersion] = useState('');
  const [sort, setSort] = useState<ContentSort>('relevance');
  const [results, setResults] = useState<CanonicalContent[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<CanonicalContent | null>(null);
  const requestId = useRef(0);

  const gameVersionOptions = useMemo<SelectFieldOption<string>[]>(() => {
    const set = new Set<string>(RECENT_MC);
    for (const version of versions.value) {
      if (!version.loader && /^\d+\.\d+(\.\d+)?$/.test(version.id)) set.add(version.id);
    }
    const list = [...set].sort(compareMcDesc);
    return [{ value: '', label: 'Any version' }, ...list.map((value) => ({ value, label: value }))];
  }, [versions.value]);

  const activeLoader = usesLoaderFilter(kind) ? loader : '';

  useEffect(() => {
    const id = ++requestId.current;
    setLoading(true);
    setError(null);
    const timer = window.setTimeout(() => {
      searchContent({
        kind,
        query: query.trim() || undefined,
        loader: activeLoader || undefined,
        gameVersion: gameVersion || undefined,
        sort,
        limit: PAGE_SIZE,
      })
        .then((page) => {
          if (id !== requestId.current) return;
          setResults(page.items);
          setTotal(page.total);
        })
        .catch((err) => {
          if (id === requestId.current) setError(err?.message || 'Could not load content.');
        })
        .finally(() => {
          if (id === requestId.current) setLoading(false);
        });
    }, SEARCH_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [kind, query, activeLoader, gameVersion, sort]);

  const loadMore = (): void => {
    if (loadingMore || loading || results.length >= total) return;
    const id = requestId.current;
    setLoadingMore(true);
    searchContent({
      kind,
      query: query.trim() || undefined,
      loader: activeLoader || undefined,
      gameVersion: gameVersion || undefined,
      sort,
      offset: results.length,
      limit: PAGE_SIZE,
    })
      .then((page) => {
        if (id !== requestId.current) return;
        setResults((current) => [...current, ...page.items]);
        setTotal(page.total);
      })
      .catch(() => {})
      .finally(() => setLoadingMore(false));
  };

  const changeKind = (next: ContentKind): void => {
    setKind(next);
    if (!usesLoaderFilter(next)) setLoader('');
  };

  const hasMore = results.length < total;

  return (
    <div class="cp-view-page">
      <div class="cp-page-header">
        <div>
          <h1>Discover</h1>
          <div class="cp-page-sub">Browse mods, packs, and shaders from Modrinth and add them to your instances.</div>
        </div>
      </div>

      <div class="cp-discover-search">
        <Input
          value={query}
          onChange={setQuery}
          icon="search"
          placeholder={`Search ${KIND_TABS.find((tab) => tab.value === kind)?.label.toLowerCase() ?? 'content'} on Modrinth`}
          trailing={
            loading ? (
              <Spinner />
            ) : query ? (
              <button class="cp-discover-clear" onClick={() => setQuery('')} aria-label="Clear search">
                <Icon name="x" size={13} />
              </button>
            ) : undefined
          }
        />
      </div>

      <div class="cp-discover-filters">
        <Segmented
          options={KIND_TABS}
          value={kind}
          onChange={changeKind}
          size="sm"
          ariaLabel="Content type"
          role="tablist"
        />
        <div class="cp-discover-filters-spacer" />
        {usesLoaderFilter(kind) && (
          <SelectField value={loader} onChange={setLoader} options={LOADER_OPTIONS} ariaLabel="Loader" width={140} />
        )}
        <SelectField
          value={gameVersion}
          onChange={setGameVersion}
          options={gameVersionOptions}
          ariaLabel="Minecraft version"
          width={150}
        />
        <SelectField value={sort} onChange={setSort} options={SORT_OPTIONS} ariaLabel="Sort by" width={165} />
      </div>

      <div class="cp-discover-count" aria-live="polite">
        {!loading && !error && total > 0 ? `${total.toLocaleString()} results` : ''}
      </div>

      {error && (
        <div class="cp-discover-empty cp-discover-empty--pad">
          <Icon name="alert" size={20} />
          <div>{error}</div>
        </div>
      )}

      {!error && loading && results.length === 0 && (
        <div class="cp-discover-grid">
          {Array.from({ length: 8 }, (_, index) => (
            <SkeletonCard key={index} />
          ))}
        </div>
      )}

      {!error && !loading && results.length === 0 && (
        <div class="cp-discover-empty cp-discover-empty--pad">
          <Icon name="search" size={20} />
          <div>No results. Try a different search or filter.</div>
        </div>
      )}

      {results.length > 0 && (
        <>
          <div class="cp-discover-grid" data-loading={loading}>
            {results.map((item) => (
              <ContentCard key={item.canonical_id} item={item} onOpen={setSelected} />
            ))}
          </div>
          {hasMore && (
            <div class="cp-discover-loadmore">
              <Button variant="secondary" onClick={loadMore} disabled={loadingMore}>
                {loadingMore ? 'Loading…' : 'Load more'}
              </Button>
            </div>
          )}
        </>
      )}

      {selected && <DetailModal item={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}
