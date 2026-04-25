import { useEffect, useMemo, useRef } from 'preact/hooks';
import type { NewInstanceLoaderMachine } from '../../machines/new-instance-loader';
import type { Catalog, LoaderComponentId } from '../../types';
import { LOADER_COMPONENT_IDS, type LoaderKey } from './defaults';

const LOADER_HOVER_PREFETCH_DELAY_MS = 140;
const LOADER_HOVER_IDLE_TIMEOUT_MS = 500;

type IdleCallbackHandle = number;
type IdleCallbackDeadline = {
  didTimeout: boolean;
  timeRemaining: () => number;
};

type IdleCapableWindow = Window & {
  requestIdleCallback?: (
    callback: (deadline: IdleCallbackDeadline) => void,
    options?: { timeout: number },
  ) => IdleCallbackHandle;
  cancelIdleCallback?: (handle: IdleCallbackHandle) => void;
};

export function useLoaderHoverPrefetch({
  source,
  mcVersionId,
  latest,
  loaderMachine,
}: {
  source: LoaderKey;
  mcVersionId: string | null;
  latest: Catalog['latest'] | null | undefined;
  loaderMachine: NewInstanceLoaderMachine;
}): {
  scheduleHoverPrefetch: (loaderKey: LoaderKey) => void;
  cancelHoverPrefetch: () => void;
} {
  const hoverPrefetchTimeoutRef = useRef<number | null>(null);
  const hoverPrefetchIdleRef = useRef<IdleCallbackHandle | null>(null);
  const prefetchedComponentsRef = useRef<Set<LoaderComponentId>>(new Set());
  const prefetchingComponentsRef = useRef<Set<LoaderComponentId>>(new Set());

  const hoverPrefetchVersions = useMemo(() => {
    const ids = new Set<string>();
    if (mcVersionId) ids.add(mcVersionId);
    if (latest?.release) ids.add(latest.release);
    if (latest?.snapshot) ids.add(latest.snapshot);
    return Array.from(ids).slice(0, 3);
  }, [mcVersionId, latest]);

  const cancelHoverPrefetch = (): void => {
    if (hoverPrefetchTimeoutRef.current != null) {
      window.clearTimeout(hoverPrefetchTimeoutRef.current);
      hoverPrefetchTimeoutRef.current = null;
    }
    const idleWindow = window as IdleCapableWindow;
    if (hoverPrefetchIdleRef.current != null && idleWindow.cancelIdleCallback) {
      idleWindow.cancelIdleCallback(hoverPrefetchIdleRef.current);
      hoverPrefetchIdleRef.current = null;
    }
  };

  const runHoverPrefetch = (componentId: LoaderComponentId): void => {
    if (prefetchedComponentsRef.current.has(componentId)) return;
    if (prefetchingComponentsRef.current.has(componentId)) return;
    prefetchingComponentsRef.current.add(componentId);
    void loaderMachine.prefetchComponent(componentId, hoverPrefetchVersions)
      .then(() => {
        prefetchedComponentsRef.current.add(componentId);
      })
      .finally(() => {
        prefetchingComponentsRef.current.delete(componentId);
      });
  };

  const scheduleHoverPrefetch = (loaderKey: LoaderKey): void => {
    if (loaderKey === 'vanilla' || loaderKey === source) return;
    const componentId = LOADER_COMPONENT_IDS[loaderKey];
    if (prefetchedComponentsRef.current.has(componentId)) return;
    if (prefetchingComponentsRef.current.has(componentId)) return;
    cancelHoverPrefetch();
    hoverPrefetchTimeoutRef.current = window.setTimeout(() => {
      hoverPrefetchTimeoutRef.current = null;
      const idleWindow = window as IdleCapableWindow;
      if (idleWindow.requestIdleCallback) {
        hoverPrefetchIdleRef.current = idleWindow.requestIdleCallback(() => {
          hoverPrefetchIdleRef.current = null;
          runHoverPrefetch(componentId);
        }, { timeout: LOADER_HOVER_IDLE_TIMEOUT_MS });
        return;
      }
      runHoverPrefetch(componentId);
    }, LOADER_HOVER_PREFETCH_DELAY_MS);
  };

  useEffect(() => () => { cancelHoverPrefetch(); }, []);

  return {
    scheduleHoverPrefetch,
    cancelHoverPrefetch,
  };
}
