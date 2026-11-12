import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Empty, IconButton, Input, Kbd, KeyLegend, Ring, Tabs } from '../../ui/Atoms';
import { ChoicePills } from '../../ui/ChoicePills';
import { Icon } from '../../ui/Icons';
import { InstanceFacts } from '../../ui/InstanceFacts';
import { SelectField } from '../../ui/Select';
import { searchContent, type ContentSearchInput } from '../../content';
import { contentRevision } from '../../content-activity';
import { versions } from '../../store';
import type { ContentKind } from '../../types-content';
import { navigate } from '../../ui-state';
import { errMessage } from '../../utils';
import { actionVerb } from './ContentAction';
import { gameVersionOptions, KIND_CATEGORIES, LOADER_OPTIONS, SORT_OPTIONS } from './filters';
import { staging } from './fit';
import { InstallConflictSheet, useInstallFlow } from './install-flow';
import { InstallTarget } from './InstallTarget';
import { ResultGhost, ResultRow } from './ResultRow';
import { FitFacts, KIND_TABS, tagLabel, usesLoaderFilter } from './shared';
import {
  category,
  clearFilters,
  fitOnly,
  gameVersion,
  kind,
  loader,
  loading,
  loadingMore,
  query,
  requestDiscoverSearch,
  requestMoreDiscoverResults,
  resultInHand,
  results,
  searchError,
  sort,
  targetInstance,
  total,
  tray,
} from './state';
import { Tray } from './Tray';
import { inHandResult, useResultKeys } from './use-result-keys';

const PAGE_SIZE = 40;
const SEARCH_DEBOUNCE_MS = 220;
const QUICK_CATEGORIES = 5;
/** Where looks are what is being chosen, a result shows its screenshot. */
const COVER_KINDS: ReadonlySet<ContentKind> = new Set(['modpack', 'resource_pack', 'shader_pack']);

/** The categories the results carry most, as one-click refinements. */
function commonCategories(): string[] {
  const counts = new Map<string, number>();
  for (const item of results.value) {
    for (const slug of item.categories) counts.set(slug, (counts.get(slug) ?? 0) + 1);
  }
  return [...counts]
    .sort((a, b) => b[1] - a[1])
    .slice(0, QUICK_CATEGORIES)
    .map(([slug]) => slug);
}

/** Three levels, each quieter than the last (what you browse, the search, its refinements), then one list. */
export function DiscoverView(): JSX.Element {
  const instance = targetInstance.value;
  const flow = useInstallFlow(instance?.id);
  const [attempt, setAttempt] = useState(0);
  const [filtersOpen, setFiltersOpen] = useState(false);
  const searchRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  const currentKind = kind.value;
  const currentCategory = category.value;
  const trimmedQuery = query.value.trim();
  const modless = instance !== null && !instance.version_display.supports_mods;
  // A pack brings its own loader and version, so the picks never narrow the packs themselves.
  const staged = staging.value;
  const fit = instance || currentKind === 'modpack' ? null : staged.fit;
  const fitting = fit !== null && (fitOnly.value ?? staged.packs.length > 0);
  const activeLoader = !usesLoaderFilter(currentKind)
    ? ''
    : instance
      ? modless
        ? ''
        : instance.version_display.loader_key
      : fitting
        ? fit.loader === 'vanilla'
          ? ''
          : fit.loader
        : loader.value;
  const activeVersion = instance
    ? instance.version_display.minecraft_label
    : fitting
      ? fit.gameVersion
      : gameVersion.value;

  const picks = tray.value;
  const tabs = useMemo(
    () =>
      KIND_TABS.map((tab) => {
        const picked = instance ? 0 : picks.filter((item) => item.kind === tab.id).length;
        if (modless && tab.id === 'mod') return { ...tab, disabled: true, title: `${instance.name} has no mod loader` };
        return picked > 0 ? { ...tab, count: picked, title: `${tab.label}: ${picked} picked` } : tab;
      }),
    [instance, modless, picks],
  );
  useEffect(() => {
    if (modless && kind.value === 'mod') kind.value = 'resource_pack';
  }, [modless]);

  const versionOptions = useMemo(() => gameVersionOptions(versions.value), [versions.value]);

  const searchInput: ContentSearchInput = {
    kind: currentKind,
    query: trimmedQuery || undefined,
    loader: activeLoader || undefined,
    gameVersion: activeVersion || undefined,
    category: currentCategory || undefined,
    sort: sort.value,
    limit: PAGE_SIZE,
    instanceId: instance?.id,
  };
  const context = [
    currentKind,
    activeLoader,
    activeVersion,
    currentCategory,
    instance?.id ?? '',
    contentRevision.value,
  ];
  const searchKey = JSON.stringify([...context, trimmedQuery, sort.value]);
  const previous = useRef<{ query: string; attempt: number } | null>(null);

  useEffect(() => {
    const before = previous.current;
    previous.current = { query: trimmedQuery, attempt };
    requestDiscoverSearch({
      key: searchKey,
      contextKey: JSON.stringify(context),
      input: searchInput,
      search: searchContent,
      errorMessage: errMessage,
      debounceMs: before && before.query !== trimmedQuery ? SEARCH_DEBOUNCE_MS : 0,
      force: before !== null && before.attempt !== attempt,
    });
  }, [searchKey, attempt]);

  useEffect(() => {
    searchRef.current?.focus({ preventScroll: true });
  }, []);

  const open = (canonicalId: string): void => navigate({ name: 'content', id: canonicalId, target: instance?.id });
  useResultKeys({ listRef, searchRef, onOpen: open });

  const changeKind = (next: ContentKind): void => {
    kind.value = next;
    if (!usesLoaderFilter(next)) loader.value = '';
    category.value = '';
  };

  const items = results.value;
  const isLoading = loading.value;
  const error = searchError.value;
  const inHand = inHandResult();
  const kindLabel = KIND_TABS.find((tab) => tab.id === currentKind)!.label.toLowerCase();
  const fitsLabel = instance
    ? ` for ${instance.version_display.summary_label}`
    : fitting
      ? ` for ${fit.loaderLabel} ${fit.gameVersion}`
      : '';
  const covers = COVER_KINDS.has(currentKind);

  // The quick refinements stay put while one of them is narrowing the results they were drawn from.
  const quick = useRef<{ kind: ContentKind; slugs: string[] }>({ kind: currentKind, slugs: [] });
  if (!currentCategory || quick.current.kind !== currentKind) {
    quick.current = { kind: currentKind, slugs: commonCategories() };
  }
  const quickSlugs =
    currentCategory && !quick.current.slugs.includes(currentCategory)
      ? [currentCategory, ...quick.current.slugs]
      : quick.current.slugs;

  const held = items.find((item) => item.canonical_id === inHand);
  const verb = held ? actionVerb(held, instance) : null;

  return (
    <div class="cp-view-page cp-discover">
      <div class="cp-discover-what">
        <Tabs tabs={tabs} active={currentKind} onSelect={changeKind} label="Content type" lead />
        {instance && <InstallTarget instance={instance} />}
      </div>

      <Input
        lead
        inputRef={searchRef}
        value={query.value}
        onChange={(value) => {
          query.value = value;
        }}
        icon="search"
        placeholder={`Search ${kindLabel}`}
        ariaLabel={`Search ${kindLabel}`}
        trailing={
          <>
            {isLoading ? (
              <Ring size={14} />
            ) : (
              !error &&
              total.value > 0 && (
                <span class="cp-discover-count" aria-live="polite">
                  {total.value.toLocaleString()} {kindLabel}
                </span>
              )
            )}
            {query.value ? (
              <IconButton
                icon="x"
                size={24}
                tooltip="Clear search"
                onClick={() => {
                  query.value = '';
                  searchRef.current?.focus();
                }}
              />
            ) : (
              <Kbd>/</Kbd>
            )}
          </>
        }
      />

      <div class="cp-discover-refine">
        {instance ? (
          <InstanceFacts inst={instance} />
        ) : (
          <>
            {fit && (
              <button
                type="button"
                class="cp-discover-fit"
                aria-pressed={fitting}
                title={
                  fitting
                    ? 'Showing only what runs with your picks. Press to browse everything.'
                    : `Show only what runs on ${fit.loaderLabel} ${fit.gameVersion}, like your picks`
                }
                onClick={() => {
                  fitOnly.value = !fitting;
                }}
              >
                <Icon name="check" size={12} stroke={2.6} />
                Fits your picks
                {fitting && <FitFacts fit={fit} />}
              </button>
            )}
            {!fitting && usesLoaderFilter(currentKind) && (
              <SelectField
                look="quiet"
                value={loader.value}
                onChange={(value) => {
                  loader.value = value;
                }}
                options={LOADER_OPTIONS}
                ariaLabel="Loader"
              />
            )}
            {!fitting && (
              <SelectField
                look="quiet"
                value={gameVersion.value}
                onChange={(value) => {
                  gameVersion.value = value;
                }}
                options={versionOptions}
                ariaLabel="Minecraft version"
              />
            )}
          </>
        )}
        <div class="cp-discover-cats" role="group" aria-label="Categories">
          {!filtersOpen &&
            quickSlugs.map((slug) => (
              <button
                key={slug}
                type="button"
                aria-pressed={currentCategory === slug}
                onClick={() => {
                  category.value = currentCategory === slug ? '' : slug;
                }}
              >
                {tagLabel(slug)}
              </button>
            ))}
          <button
            type="button"
            class="cp-discover-more"
            aria-expanded={filtersOpen}
            aria-pressed={filtersOpen}
            onClick={() => setFiltersOpen((value) => !value)}
          >
            <Icon name="sliders" size={13} stroke={2} />
            Filters
          </button>
        </div>
        <span class="cp-head-fill" />
        <SelectField
          look="ghost"
          value={sort.value}
          onChange={(value) => {
            sort.value = value;
          }}
          options={SORT_OPTIONS}
          ariaLabel="Sort by"
        />
      </div>

      {filtersOpen && (
        <ChoicePills
          ariaLabel="Category"
          value={currentCategory}
          onChange={(value) => {
            category.value = value;
          }}
          options={[
            { value: '', label: 'All' },
            ...KIND_CATEGORIES[currentKind].map((slug) => ({ value: slug, label: tagLabel(slug) })),
          ]}
        />
      )}

      {error ? (
        <Empty
          eyes="x"
          title="Search did not load"
          actions={
            <Button variant="secondary" icon="refresh" onClick={() => setAttempt((value) => value + 1)}>
              Try again
            </Button>
          }
        >
          {error}
        </Empty>
      ) : isLoading && items.length === 0 ? (
        <div class="cp-discover-list" data-covers={covers}>
          {Array.from({ length: 6 }, (_, index) => (
            <ResultGhost key={index} covers={covers} />
          ))}
        </div>
      ) : items.length === 0 ? (
        <Empty
          title="Nothing matched"
          actions={
            (trimmedQuery || activeLoader || activeVersion || currentCategory) && (
              <Button
                variant="secondary"
                onClick={() => {
                  clearFilters();
                  fitOnly.value = false;
                }}
              >
                Clear filters
              </Button>
            )
          }
        >
          {trimmedQuery
            ? `No ${kindLabel} named “${trimmedQuery}”${fitsLabel}.`
            : `No ${kindLabel}${fitsLabel} to show here.`}
        </Empty>
      ) : (
        <>
          <div class="cp-discover-list" role="list" ref={listRef} data-loading={isLoading} data-covers={covers}>
            {items.map((item) => (
              <ResultRow
                key={item.canonical_id}
                item={item}
                browsing={currentKind}
                covers={covers}
                instance={instance}
                flow={flow}
                inHand={item.canonical_id === inHand}
                onHold={() => {
                  resultInHand.value = item.canonical_id;
                }}
                onOpen={() => open(item.canonical_id)}
              />
            ))}
          </div>
          {items.length < total.value && (
            <Button
              variant="secondary"
              busy={loadingMore.value}
              disabled={isLoading || loadingMore.value}
              onClick={() =>
                requestMoreDiscoverResults({
                  key: searchKey,
                  input: { ...searchInput, offset: items.length },
                  search: searchContent,
                })
              }
            >
              Load more
            </Button>
          )}
          <KeyLegend
            keys={[
              [['↑', '↓'], 'move'],
              ...(verb ? ([[['↵'], verb]] as const) : []),
              [['→'], 'details'],
              [['/'], 'search'],
            ]}
          />
        </>
      )}

      <InstallConflictSheet flow={flow} />
      <Tray />
    </div>
  );
}
