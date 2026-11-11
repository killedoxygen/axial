import type { JSX } from 'preact';
import { useEffect, useRef } from 'preact/hooks';
import { Chip } from '../../ui/Atoms';
import { Art } from '../../ui/Art';
import type { SearchHit } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';
import { ContentAction } from './ContentAction';
import { scheduleDetailPrefetch } from './detail-cache';
import type { InstallFlow } from './install-flow';
import { ContentStats, KIND_ICON, tagLabel } from './shared';

const PREFETCH_HOVER_MS = 150;
const PREFETCH_FOCUS_MS = 250;

/**
 * One result, read left to right: what it is (art, name, author, kinds) over one line of what it does, how used it
 * is, and the one action on the page's right edge. The row opens its details; the action never does.
 */
export function ResultRow({
  item,
  instance,
  flow,
  inHand,
  onHold,
  onOpen,
}: {
  item: SearchHit;
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
      <Art url={item.icon_url} icon={KIND_ICON[item.kind]} size={44} />
      <div class="cp-discover-row-id">
        <div class="cp-discover-row-name">
          <h3 title={item.title}>
            <button type="button" onClick={onOpen} onFocus={() => prefetch(PREFETCH_FOCUS_MS)} onBlur={stopPrefetch}>
              {item.title}
            </button>
          </h3>
          {item.author && <span class="cp-discover-by">by {item.author}</span>}
          <span class="cp-chips">
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
export function ResultGhost(): JSX.Element {
  return (
    <div class="cp-row cp-discover-row" data-ghost aria-hidden="true">
      <span class="cp-art" />
      <div class="cp-discover-row-id">
        <span />
        <span />
      </div>
    </div>
  );
}
