import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Chip } from '../../ui/Atoms';
import { Art } from '../../ui/Art';
import type { ContentKind, SearchHit } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { ContentAction } from './ContentAction';
import { scheduleDetailPrefetch } from './detail-cache';
import type { InstallFlow } from './install-flow';
import { ContentStats, KIND_ICON, KIND_NOUN, tagLabel } from './shared';

const PREFETCH_HOVER_MS = 150;
const PREFETCH_FOCUS_MS = 250;

/**
 * Where looks are what is being chosen, a result leads with the screenshot its author chose, its own art in the
 * corner. With no screenshot the art stands alone in the same holder, so the names still share one edge.
 */
function Cover({ item }: { item?: SearchHit }): JSX.Element {
  const [failed, setFailed] = useState<string | null>(null);
  const url = item?.cover_url;
  const shown = url !== undefined && failed !== url;
  return (
    <span class="cp-discover-cover" data-shown={shown} aria-hidden="true">
      {shown && (
        <img
          src={url}
          alt=""
          loading="lazy"
          decoding="async"
          draggable={false}
          onLoad={(event) => {
            event.currentTarget.dataset.loaded = 'true';
          }}
          onError={() => setFailed(url)}
        />
      )}
      {item && <Art url={item.icon_url} icon={KIND_ICON[item.kind]} size={shown ? 28 : 40} />}
    </span>
  );
}

/**
 * One result, read left to right: what it is (art, name, author, kinds) over one line of what it does, how used it
 * is, and the one action on the page's right edge. The row opens its details; the action never does.
 */
export function ResultRow({
  item,
  browsing,
  covers,
  instance,
  flow,
  inHand,
  onHold,
  onOpen,
}: {
  item: SearchHit;
  /** The kind being browsed; a project of several kinds is listed under each and says which it mainly is. */
  browsing: ContentKind;
  covers: boolean;
  instance: EnrichedInstance | null;
  flow: InstallFlow;
  inHand: boolean;
  onHold: () => void;
  onOpen: () => void;
}): JSX.Element {
  const cancelPrefetch = useRef<(() => void) | null>(null);
  const prefetch = (delayMs: number): void => {
    cancelPrefetch.current?.();
    cancelPrefetch.current = scheduleDetailPrefetch(item.canonical_id, delayMs);
  };
  const stopPrefetch = (): void => {
    cancelPrefetch.current?.();
    cancelPrefetch.current = null;
  };
  useEffect(() => stopPrefetch, []);
  useEffect(() => {
    if (inHand) prefetch(PREFETCH_HOVER_MS);
    else stopPrefetch();
  }, [inHand]);

  return (
    <article
      class="cp-row cp-discover-row"
      role="listitem"
      data-result={item.canonical_id}
      data-active={inHand}
      onMouseMove={inHand ? undefined : onHold}
      onClick={(event) => {
        if (!(event.target as Element).closest('button, a')) onOpen();
      }}
    >
      {covers ? <Cover item={item} /> : <Art url={item.icon_url} icon={KIND_ICON[item.kind]} size={44} />}
      <div class="cp-discover-row-id">
        <div class="cp-discover-row-name">
          <h3 title={item.title}>
            <button type="button" onClick={onOpen} onFocus={() => prefetch(PREFETCH_FOCUS_MS)} onBlur={stopPrefetch}>
              {item.title}
            </button>
          </h3>
          {item.author && <span class="cp-discover-by">by {item.author}</span>}
          <span class="cp-chips">
            {item.kind !== browsing && (
              <Chip tone="accent" icon={KIND_ICON[item.kind]} title={`Installs as a ${KIND_NOUN[item.kind]}`}>
                {tagLabel(KIND_NOUN[item.kind])}
              </Chip>
            )}
            {item.categories.slice(0, 3).map((category) => (
              <Chip key={category}>{tagLabel(category)}</Chip>
            ))}
          </span>
        </div>
        <p>{item.summary}</p>
      </div>
      <ContentStats downloads={item.downloads} follows={item.follows} />
      <span class="cp-discover-act">
        <ContentAction item={item} instance={instance} flow={flow} inHand={inHand} />
      </span>
    </article>
  );
}

/** A row's shape while the first page is on its way. */
export function ResultGhost({ covers }: { covers: boolean }): JSX.Element {
  return (
    <div class="cp-row cp-discover-row" data-ghost aria-hidden="true">
      {covers ? <Cover /> : <span class="cp-art" />}
      <div class="cp-discover-row-id">
        <span />
        <span />
      </div>
    </div>
  );
}
