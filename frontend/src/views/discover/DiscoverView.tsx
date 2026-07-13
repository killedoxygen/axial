import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Input, Pill } from '../../ui/Atoms';
import { Segmented } from '../../ui/Segmented';
import { SelectField, type SelectFieldOption } from '../../ui/Select';
import { Icon } from '../../ui/Icons';
import { versions } from '../../store';
import { navigate } from '../../ui-state';
import { searchContent, type ContentSearchInput } from '../../content';
import { formatAge, formatCount } from '../../format';
import { contentRevision } from '../../content-activity';
import { errMessage } from '../../utils';
import type { ContentKind, ContentSort, SearchHit } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { TargetBar } from './TargetBar';
import { Tray } from './Tray';
import { ModpackPicker } from './ModpackPicker';
import { setUpModpack } from './actions';
import { InstallConflictSheet, useInstallFlow, type InstallFlow } from './install-flow';
import {
  category,
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
  stageContent,
  targetInstance,
  total,
  unstage,
} from './state';
import {
  compareMcDesc,
  ContentIcon,
  isAddable,
  KIND_TABS,
  SkeletonCard,
  Spinner,
  tagLabel,
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

const KIND_CATEGORIES: Record<ContentKind, string[]> = {
  mod: [
    'adventure',
    'decoration',
    'economy',
    'equipment',
    'food',
    'game-mechanics',
    'library',
    'magic',
    'management',
    'minigame',
    'mobs',
    'optimization',
    'social',
    'storage',
    'technology',
    'transportation',
    'utility',
    'worldgen',
  ],
  modpack: [
    'adventure',
    'challenging',
    'combat',
    'kitchen-sink',
    'lightweight',
    'magic',
    'multiplayer',
    'optimization',
    'quests',
    'technology',
  ],
  resource_pack: [
    'combat',
    'decoration',
    'modded',
    'realistic',
    'simplistic',
    'themed',
    'tweaks',
    'utility',
    'vanilla-like',
  ],
  shader_pack: [
    'atmosphere',
    'bloom',
    'colored-lighting',
    'foliage',
    'pbr',
    'reflections',
    'shadows',
    'potato',
    'low',
    'medium',
    'high',
  ],
};

const RECENT_MC = ['1.21.6', '1.21.5', '1.21.4', '1.21.3', '1.21.1', '1.21', '1.20.6', '1.20.4', '1.20.1'];
const PAGE_SIZE = 40;
const SEARCH_DEBOUNCE_MS = 220;

function CardAction({
  item,
  instance,
  flow,
}: {
  item: SearchHit;
  instance: EnrichedInstance | null;
  flow: InstallFlow;
}): JSX.Element | null {
  const [busy, setBusy] = useState(false);
  const [pickingPack, setPickingPack] = useState(false);
  const staged = isStaged(item.canonical_id);

  if (item.install_state === 'installed') {
    return (
      <span class="cp-discover-installed" title="Already in this instance">
        <Icon name="download" size={13} />
        Installed
      </span>
    );
  }

  if (item.kind === 'modpack') {
    if (instance) {
      return (
        <>
          <Button
            variant="secondary"
            size="sm"
            icon="plus"
            title={`Choose files for ${instance.name}`}
            onClick={(event) => {
              event.stopPropagation();
              setPickingPack(true);
            }}
          >
            Choose
          </Button>
          <ModpackPicker
            open={pickingPack}
            instanceId={instance.id}
            canonicalId={item.canonical_id}
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
        disabled={busy}
        title="Create an instance from this pack"
        onClick={(event) => {
          event.stopPropagation();
          setBusy(true);
          void setUpModpack(item.canonical_id, undefined, item.icon_url).finally(() => setBusy(false));
        }}
      >
        {busy ? 'Working…' : 'Set up'}
      </Button>
    );
  }

  if (!isAddable(item.kind)) return null;

  if (instance) {
    return (
      <Button
        variant="secondary"
        size="sm"
        icon={busy ? undefined : 'plus'}
        disabled={busy}
        title={`Add to ${instance.name}`}
        onClick={(event) => {
          event.stopPropagation();
          setBusy(true);
          void flow
            .add([{ canonical_id: item.canonical_id, kind: item.kind }], item.title)
            .finally(() => setBusy(false));
        }}
      >
        {busy ? <Spinner size={12} /> : 'Add'}
      </Button>
    );
  }

  return (
    <Button
      variant={staged ? 'primary' : 'secondary'}
      size="sm"
      icon={staged ? 'check' : 'plus'}
      title={staged ? 'Remove from selection' : 'Add to selection'}
      onClick={(event) => {
        event.stopPropagation();
        if (staged) unstage(item.canonical_id);
        else stageContent(item);
      }}
    >
      {staged ? 'Staged' : 'Stage'}
    </Button>
  );
}

function ContentCard({
  item,
  instance,
  flow,
}: {
  item: SearchHit;
  instance: EnrichedInstance | null;
  flow: InstallFlow;
}): JSX.Element {
  const open = (): void => navigate({ name: 'content', id: item.canonical_id, target: instance?.id });

  return (
    <article
      class="cp-discover-card"
      data-staged={isStaged(item.canonical_id)}
      role="button"
      tabIndex={0}
      aria-label={item.title}
      onClick={open}
      onKeyDown={(event: KeyboardEvent) => {
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          open();
        }
      }}
    >
      <div class="cp-discover-card-icon" aria-hidden="true">
        <ContentIcon url={item.icon_url} kind={item.kind} />
      </div>
      <div class="cp-discover-card-main">
        <div class="cp-discover-card-head">
          <h3 class="cp-discover-card-title" title={item.title}>
            {item.title}
          </h3>
          {item.author && <span class="cp-discover-card-author">by {item.author}</span>}
        </div>
        <p class="cp-discover-card-summary">{item.summary}</p>
        <div class="cp-discover-card-tags">
          {item.categories.slice(0, 3).map((category) => (
            <Pill key={category}>{tagLabel(category)}</Pill>
          ))}
        </div>
        <div class="cp-discover-card-stats">
          <span class="cp-discover-stat" title={`${item.downloads.toLocaleString()} downloads`}>
            <Icon name="download" size={12} />
            {formatCount(item.downloads)}
          </span>
          <span class="cp-discover-stat" title={`${item.follows.toLocaleString()} followers`}>
            <Icon name="user" size={12} />
            {formatCount(item.follows)}
          </span>
          {item.updated && (
            <span class="cp-discover-stat" title={`Updated ${formatAge(item.updated)}`}>
              <Icon name="clock" size={12} />
              {formatAge(item.updated)}
            </span>
          )}
        </div>
      </div>
      <div class="cp-discover-card-action">
        <CardAction item={item} instance={instance} flow={flow} />
      </div>
    </article>
  );
}

export function DiscoverView(): JSX.Element {
  const instance = targetInstance.value;
  const requestId = useRef(0);
  const [attempt, setAttempt] = useState(0);
  const flow = useInstallFlow(instance?.id);

  const activeLoader = instance
    ? instance.version_display.supports_mods
      ? instance.version_display.loader_key
      : ''
    : usesLoaderFilter(kind.value)
      ? loader.value
      : '';
  const activeVersion = instance ? instance.version_display.minecraft_label : gameVersion.value;

  const tabs = useMemo(() => {
    if (!instance) return KIND_TABS;
    return KIND_TABS.map((tab) => {
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
  const currentCategory = category.value;
  const currentContentRevision = contentRevision.value;
  const [filtersOpen, setFiltersOpen] = useState(false);

  const searchInput = (offset?: number): ContentSearchInput => ({
    kind: currentKind,
    query: currentQuery.trim() || undefined,
    loader: usesLoaderFilter(currentKind) ? activeLoader || undefined : undefined,
    gameVersion: activeVersion || undefined,
    category: currentCategory || undefined,
    sort: currentSort,
    offset,
    limit: PAGE_SIZE,
    instanceId: instance?.id,
  });

  useEffect(() => {
    const id = ++requestId.current;
    loading.value = true;
    loadingMore.value = false;
    searchError.value = null;
    const timer = window.setTimeout(() => {
      searchContent(searchInput())
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
    return () => {
      window.clearTimeout(timer);
      requestId.current += 1;
    };
  }, [
    currentKind,
    currentQuery,
    activeLoader,
    activeVersion,
    currentCategory,
    currentSort,
    instance?.id,
    attempt,
    currentContentRevision,
  ]);

  const loadMore = (): void => {
    if (loadingMore.value || loading.value || results.value.length >= total.value) return;
    const id = requestId.current;
    loadingMore.value = true;
    searchContent(searchInput(results.value.length))
      .then((page) => {
        if (id !== requestId.current) return;
        results.value = [...results.value, ...page.items];
        total.value = page.total;
      })
      .catch(() => {})
      .finally(() => {
        if (id === requestId.current) loadingMore.value = false;
      });
  };

  const changeKind = (next: ContentKind): void => {
    kind.value = next;
    if (!usesLoaderFilter(next)) loader.value = '';
    category.value = '';
  };

  const items = results.value;
  const isLoading = loading.value;
  const error = searchError.value;
  const hasMore = items.length < total.value;
  const kindLabel = KIND_TABS.find((tab) => tab.value === currentKind)?.label.toLowerCase() ?? 'content';
  const filtered = Boolean(currentQuery || loader.value || gameVersion.value || currentCategory);
  const categories = KIND_CATEGORIES[currentKind];

  return (
    <div class="cp-view-page">
      <div class="cp-page-header">
        <div>
          <h1>Discover</h1>
          <div class="cp-page-sub" aria-live="polite">
            {isLoading && items.length === 0
              ? `Searching ${kindLabel}…`
              : error
                ? 'Search is unavailable.'
                : total.value > 0
                  ? `${total.value.toLocaleString()} ${kindLabel}${
                      instance ? ` that fit ${instance.version_display.summary_label}` : ' on Modrinth'
                    }`
                  : `Browse ${kindLabel} from Modrinth.`}
          </div>
        </div>
        {instance && (
          <div class="cp-page-header-side">
            <TargetBar instance={instance} />
          </div>
        )}
      </div>

      <div class="cp-discover-controls">
        <Input
          value={currentQuery}
          onChange={(value) => {
            query.value = value;
          }}
          icon="search"
          placeholder={`Search ${kindLabel}`}
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

        <div class="cp-discover-filters">
          <Segmented
            options={tabs}
            value={currentKind}
            onChange={changeKind}
            size="sm"
            ariaLabel="Content type"
            role="tablist"
          />

          <div class="cp-discover-bar-spacer" />

          {instance ? (
            <span class="cp-discover-locked" title="Set by the instance you are adding to">
              <Icon name="cube" size={12} />
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
                  width={108}
                />
              )}
              <SelectField
                value={gameVersion.value}
                onChange={(value) => {
                  gameVersion.value = value;
                }}
                options={gameVersionOptions}
                ariaLabel="Minecraft version"
                width={116}
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
            width={136}
          />
          <button
            type="button"
            class="cp-discover-more-filters"
            data-active={filtersOpen || Boolean(currentCategory)}
            aria-expanded={filtersOpen}
            title="More filters"
            onClick={() => setFiltersOpen((open) => !open)}
          >
            <Icon name="sliders" size={14} stroke={2} />
            Filters
            {currentCategory && <span class="cp-discover-more-filters-dot" aria-hidden="true" />}
          </button>
        </div>

        {filtersOpen && (
          <div class="cp-discover-categories" role="radiogroup" aria-label="Category">
            <button
              type="button"
              class="cp-discover-cat"
              role="radio"
              aria-checked={!currentCategory}
              data-active={!currentCategory}
              onClick={() => {
                category.value = '';
              }}
            >
              All
            </button>
            {categories.map((slug) => (
              <button
                key={slug}
                type="button"
                class="cp-discover-cat"
                role="radio"
                aria-checked={currentCategory === slug}
                data-active={currentCategory === slug}
                onClick={() => {
                  category.value = currentCategory === slug ? '' : slug;
                }}
              >
                {tagLabel(slug)}
              </button>
            ))}
          </div>
        )}
      </div>

      {error ? (
        <div class="cp-resource-empty">
          <span>
            <Icon name="alert" size={20} />
          </span>
          <strong>Could not reach Modrinth</strong>
          <p>{error}</p>
          <div class="cp-mods-empty-actions">
            <Button variant="secondary" size="sm" icon="refresh" onClick={() => setAttempt((value) => value + 1)}>
              Try again
            </Button>
          </div>
        </div>
      ) : isLoading && items.length === 0 ? (
        <div class="cp-discover-grid">
          {Array.from({ length: 6 }, (_, index) => (
            <SkeletonCard key={index} />
          ))}
        </div>
      ) : items.length === 0 ? (
        <div class="cp-resource-empty">
          <span>
            <Icon name="search" size={20} />
          </span>
          <strong>Nothing matched</strong>
          <p>
            {currentQuery
              ? `No ${kindLabel} named “${currentQuery}”${
                  instance ? ` fit ${instance.version_display.summary_label}` : ''
                }.`
              : `No ${kindLabel} to show here.`}
          </p>
          {filtered && (
            <div class="cp-mods-empty-actions">
              <Button
                variant="secondary"
                size="sm"
                onClick={() => {
                  query.value = '';
                  loader.value = '';
                  gameVersion.value = '';
                  category.value = '';
                }}
              >
                Clear filters
              </Button>
            </div>
          )}
        </div>
      ) : (
        <>
          <div class="cp-discover-grid" data-loading={isLoading}>
            {items.map((item) => (
              <ContentCard key={item.canonical_id} item={item} instance={instance} flow={flow} />
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

      <InstallConflictSheet flow={flow} />
      <Tray />
    </div>
  );
}
