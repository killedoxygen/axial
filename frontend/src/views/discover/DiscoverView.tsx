import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Input } from '../../ui/Atoms';
import { Segmented } from '../../ui/Segmented';
import { SelectField, type SelectFieldOption } from '../../ui/Select';
import { Icon } from '../../ui/Icons';
import { versions } from '../../store';
import { navigate } from '../../ui-state';
import { searchContent } from '../../content';
import { errMessage } from '../../utils';
import type { ContentKind, ContentSort, SearchHit } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { TargetBar } from './TargetBar';
import { Tray } from './Tray';
import { addToInstance, createFromModpack } from './actions';
import {
  gameVersion,
  isStaged,
  kind,
  loading,
  loadingMore,
  loader,
  query,
  results,
  searchError,
  sort,
  stage,
  targetInstance,
  total,
  tray,
  unstage,
} from './state';
import {
  compareMcDesc,
  ContentIcon,
  formatCount,
  isAddable,
  KIND_TABS,
  SkeletonCard,
  Spinner,
  usesLoaderFilter,
} from './shared';

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

/**
 * The action on a card. With an instance targeted it installs straight away —
 * that is the "I just want this one mod in this game" case, and it should cost
 * one click. With nothing targeted it stages instead, because there is nowhere
 * to install to yet and picking a set is how you get somewhere to put it.
 */
function CardAction({ item, instance }: { item: SearchHit; instance: EnrichedInstance | null }): JSX.Element {
  const [busy, setBusy] = useState(false);
  const staged = isStaged(item.canonical_id);
  const installed = item.install_state === 'installed';

  const act = async (event: MouseEvent): Promise<void> => {
    event.stopPropagation();
    if (busy || installed) return;

    if (item.kind === 'modpack') {
      setBusy(true);
      await createFromModpack(item.canonical_id);
      setBusy(false);
      return;
    }

    if (instance) {
      setBusy(true);
      await addToInstance(instance.id, [{ canonical_id: item.canonical_id, kind: item.kind }], item.title);
      setBusy(false);
      return;
    }

    if (staged) unstage(item.canonical_id);
    else stage({ canonical_id: item.canonical_id, kind: item.kind, title: item.title, icon_url: item.icon_url });
  };

  if (installed) {
    return (
      <span class="cp-discover-card-action cp-discover-card-action--done" title="Already in this instance">
        <Icon name="check" size={13} /> Installed
      </span>
    );
  }

  const label = item.kind === 'modpack' ? 'Set up' : instance ? 'Add' : staged ? 'Staged' : 'Stage';
  const icon = item.kind === 'modpack' ? 'sparkles' : instance ? 'plus' : staged ? 'check' : 'plus';

  return (
    <button
      class="cp-discover-card-action"
      data-staged={staged}
      onClick={act}
      disabled={busy}
      title={
        item.kind === 'modpack'
          ? 'Create an instance from this pack'
          : instance
            ? `Add to ${instance.name}`
            : staged
              ? 'Remove from selection'
              : 'Add to selection'
      }
    >
      {busy ? <Spinner size={12} /> : <Icon name={icon} size={13} />}
      {busy ? 'Working…' : label}
    </button>
  );
}

function ContentCard({ item, instance }: { item: SearchHit; instance: EnrichedInstance | null }): JSX.Element {
  const open = (): void => {
    navigate({ name: 'content', id: item.canonical_id, target: instance?.id });
  };

  return (
    <div class="cp-discover-card" data-staged={isStaged(item.canonical_id)}>
      <button class="cp-discover-card-open" onClick={open} aria-label={`Open ${item.title}`}>
        <div class="cp-discover-card-icon" aria-hidden="true">
          <ContentIcon url={item.icon_url} kind={item.kind} />
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
      {(isAddable(item.kind) || item.kind === 'modpack') && <CardAction item={item} instance={instance} />}
    </div>
  );
}

export function DiscoverView(): JSX.Element {
  const instance = targetInstance.value;
  const requestId = useRef(0);

  // A targeted instance dictates the filters; they are facts about where the
  // content is going, not preferences.
  const activeLoader = instance
    ? instance.version_display.supports_mods
      ? instance.version_display.loader_key
      : ''
    : usesLoaderFilter(kind.value)
      ? loader.value
      : '';
  const activeVersion = instance ? instance.version_display.minecraft_label : gameVersion.value;

  // Do not offer what the target cannot take: a vanilla instance has nowhere to
  // put a mod, and a modpack is an instance rather than something added to one.
  const tabs = useMemo(() => {
    if (!instance) return KIND_TABS;
    return KIND_TABS.map((tab) => {
      if (tab.value === 'modpack') {
        return { ...tab, disabled: true, title: 'A modpack is set up as its own instance' };
      }
      if (tab.value === 'mod' && !instance.version_display.supports_mods) {
        return { ...tab, disabled: true, title: `${instance.name} has no mod loader` };
      }
      return tab;
    });
  }, [instance]);

  useEffect(() => {
    const blocked = tabs.find((tab) => tab.value === kind.value)?.disabled;
    if (blocked) kind.value = 'resource_pack';
  }, [tabs]);

  const gameVersionOptions = useMemo<SelectFieldOption<string>[]>(() => {
    const set = new Set<string>(RECENT_MC);
    for (const version of versions.value) {
      if (!version.loader && /^\d+\.\d+(\.\d+)?$/.test(version.id)) set.add(version.id);
    }
    const list = [...set].sort(compareMcDesc);
    return [{ value: '', label: 'Any version' }, ...list.map((value) => ({ value, label: value }))];
  }, [versions.value]);

  const currentKind = kind.value;
  const currentQuery = query.value;
  const currentSort = sort.value;

  useEffect(() => {
    const id = ++requestId.current;
    loading.value = true;
    searchError.value = null;
    const timer = window.setTimeout(() => {
      searchContent({
        kind: currentKind,
        query: currentQuery.trim() || undefined,
        loader: usesLoaderFilter(currentKind) ? activeLoader || undefined : undefined,
        gameVersion: activeVersion || undefined,
        sort: currentSort,
        limit: PAGE_SIZE,
        instanceId: instance?.id,
      })
        .then((page) => {
          if (id !== requestId.current) return;
          results.value = page.items;
          total.value = page.total;
        })
        .catch((error) => {
          if (id === requestId.current) searchError.value = errMessage(error);
        })
        .finally(() => {
          if (id === requestId.current) loading.value = false;
        });
    }, SEARCH_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [currentKind, currentQuery, activeLoader, activeVersion, currentSort, instance?.id]);

  const loadMore = (): void => {
    if (loadingMore.value || loading.value || results.value.length >= total.value) return;
    const id = requestId.current;
    loadingMore.value = true;
    searchContent({
      kind: currentKind,
      query: currentQuery.trim() || undefined,
      loader: usesLoaderFilter(currentKind) ? activeLoader || undefined : undefined,
      gameVersion: activeVersion || undefined,
      sort: currentSort,
      offset: results.value.length,
      limit: PAGE_SIZE,
      instanceId: instance?.id,
    })
      .then((page) => {
        if (id !== requestId.current) return;
        results.value = [...results.value, ...page.items];
        total.value = page.total;
      })
      .catch(() => {})
      .finally(() => {
        loadingMore.value = false;
      });
  };

  const changeKind = (next: ContentKind): void => {
    kind.value = next;
    if (!usesLoaderFilter(next)) loader.value = '';
  };

  const items = results.value;
  const isLoading = loading.value;
  const error = searchError.value;
  const hasMore = items.length < total.value;
  const kindLabel = KIND_TABS.find((tab) => tab.value === currentKind)?.label.toLowerCase() ?? 'content';

  return (
    <div class="cp-view-page cp-discover" data-tray={tray.value.length > 0}>
      <div class="cp-page-header">
        <div>
          <h1>Discover</h1>
          <div class="cp-page-sub">
            {instance
              ? `Showing ${kindLabel} that work with ${instance.version_display.summary_label}.`
              : 'Browse mods, packs, and shaders from Modrinth.'}
          </div>
        </div>
      </div>

      {instance && <TargetBar instance={instance} />}

      <div class="cp-discover-search">
        <Input
          value={currentQuery}
          onChange={(value) => {
            query.value = value;
          }}
          icon="search"
          placeholder={`Search ${kindLabel} on Modrinth`}
          trailing={
            isLoading ? (
              <Spinner />
            ) : currentQuery ? (
              <button
                class="cp-discover-clear"
                onClick={() => {
                  query.value = '';
                }}
                aria-label="Clear search"
              >
                <Icon name="x" size={13} />
              </button>
            ) : undefined
          }
        />
      </div>

      <div class="cp-discover-filters">
        <Segmented
          options={tabs}
          value={currentKind}
          onChange={changeKind}
          size="sm"
          ariaLabel="Content type"
          role="tablist"
        />
        <div class="cp-discover-filters-spacer" />
        {instance ? (
          <span class="cp-discover-locked" title="Set by the instance you are adding to">
            <Icon name="shield-check" size={11} />
            {instance.version_display.summary_label}
          </span>
        ) : (
          <>
            {usesLoaderFilter(currentKind) && (
              <SelectField
                value={loader.value}
                onChange={(value) => {
                  loader.value = value;
                }}
                options={LOADER_OPTIONS}
                ariaLabel="Loader"
                width={140}
              />
            )}
            <SelectField
              value={gameVersion.value}
              onChange={(value) => {
                gameVersion.value = value;
              }}
              options={gameVersionOptions}
              ariaLabel="Minecraft version"
              width={150}
            />
          </>
        )}
        <SelectField
          value={currentSort}
          onChange={(value) => {
            sort.value = value;
          }}
          options={SORT_OPTIONS}
          ariaLabel="Sort by"
          width={165}
        />
      </div>

      <div class="cp-discover-count" aria-live="polite">
        {!isLoading && !error && total.value > 0 ? `${total.value.toLocaleString()} results` : ''}
      </div>

      {error && (
        <div class="cp-discover-empty cp-discover-empty--pad">
          <Icon name="alert" size={20} />
          <div>{error}</div>
        </div>
      )}

      {!error && isLoading && items.length === 0 && (
        <div class="cp-discover-grid">
          {Array.from({ length: 8 }, (_, index) => (
            <SkeletonCard key={index} />
          ))}
        </div>
      )}

      {!error && !isLoading && items.length === 0 && (
        <div class="cp-discover-empty cp-discover-empty--pad">
          <Icon name="search" size={20} />
          <div>No results. Try a different search or filter.</div>
        </div>
      )}

      {items.length > 0 && (
        <>
          <div class="cp-discover-grid" data-loading={isLoading}>
            {items.map((item) => (
              <ContentCard key={item.canonical_id} item={item} instance={instance} />
            ))}
          </div>
          {hasMore && (
            <div class="cp-discover-loadmore">
              <Button variant="secondary" onClick={loadMore} disabled={loadingMore.value}>
                {loadingMore.value ? 'Loading…' : 'Load more'}
              </Button>
            </div>
          )}
        </>
      )}

      <Tray />
    </div>
  );
}
