import type { JSX, RefObject } from 'preact';
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { local, saveLocalState } from '../state';
import type { OverlayPosition } from '../types';

const DRAG_IGNORE_SELECTOR = 'input, textarea, select, button, a, [data-drag-ignore="true"]';

interface OverlaySize {
  width: number;
  height: number;
}

interface DragState {
  startClientX: number;
  startClientY: number;
  origin: OverlayPosition;
  size: OverlaySize;
}

export interface DraggableOverlayOptions {
  id: string;
  enabled: boolean;
  clampMargin?: number;
}

export interface DraggableOverlayResult<T extends HTMLElement> {
  surfaceRef: RefObject<T>;
  dragHandleProps: {
    onPointerDown: (event: JSX.TargetedPointerEvent<HTMLElement>) => void;
  };
  isDragging: boolean;
  isPositioned: boolean;
  style: JSX.CSSProperties | undefined;
}

function isOverlayPosition(value: unknown): value is OverlayPosition {
  if (!value || typeof value !== 'object') return false;
  const candidate = value as Partial<OverlayPosition>;
  return Number.isFinite(candidate.x) && Number.isFinite(candidate.y);
}

function storedPosition(id: string): OverlayPosition | null {
  const value = local.overlayPositions?.[id];
  return isOverlayPosition(value) ? value : null;
}

function clampAxis(value: number, viewportSize: number, surfaceSize: number, margin: number): number {
  const widestVisibleOrigin = Math.max(0, viewportSize - surfaceSize);
  const min = Math.min(margin, widestVisibleOrigin);
  const max = Math.max(min, viewportSize - surfaceSize - margin);
  return Math.min(Math.max(value, min), max);
}

function clampPosition(point: OverlayPosition, size: OverlaySize, margin: number): OverlayPosition {
  return {
    x: clampAxis(point.x, window.innerWidth, size.width, margin),
    y: clampAxis(point.y, window.innerHeight, size.height, margin),
  };
}

function positionsMatch(a: OverlayPosition, b: OverlayPosition): boolean {
  return Math.round(a.x) === Math.round(b.x) && Math.round(a.y) === Math.round(b.y);
}

function persistPosition(id: string, point: OverlayPosition): void {
  local.overlayPositions = {
    ...(local.overlayPositions || {}),
    [id]: {
      x: Math.round(point.x),
      y: Math.round(point.y),
    },
  };
  saveLocalState();
}

export function useDraggableOverlay<T extends HTMLElement>({
  id,
  enabled,
  clampMargin = 16,
}: DraggableOverlayOptions): DraggableOverlayResult<T> {
  const surfaceRef = useRef<T>(null);
  const dragRef = useRef<DragState | null>(null);
  const cleanupDragRef = useRef<(() => void) | null>(null);
  const frameRef = useRef<number | null>(null);
  const [position, setPosition] = useState<OverlayPosition | null>(() => storedPosition(id));
  const [isDragging, setIsDragging] = useState(false);
  const positionRef = useRef<OverlayPosition | null>(position);

  useEffect(() => {
    if (dragRef.current) return;
    positionRef.current = position;
  }, [position]);

  const applyPosition = useCallback((point: OverlayPosition): void => {
    const el = surfaceRef.current;
    if (!el) return;
    el.style.position = 'absolute';
    el.style.left = `${Math.round(point.x)}px`;
    el.style.top = `${Math.round(point.y)}px`;
  }, []);

  const queuePosition = useCallback((point: OverlayPosition): void => {
    positionRef.current = point;
    if (frameRef.current !== null) return;
    frameRef.current = window.requestAnimationFrame(() => {
      frameRef.current = null;
      const next = positionRef.current;
      if (next) applyPosition(next);
    });
  }, [applyPosition]);

  const cancelQueuedPosition = useCallback((): void => {
    if (frameRef.current === null) return;
    window.cancelAnimationFrame(frameRef.current);
    frameRef.current = null;
  }, []);

  useEffect(() => {
    if (!enabled) {
      cleanupDragRef.current?.();
      cleanupDragRef.current = null;
      dragRef.current = null;
      cancelQueuedPosition();
      setIsDragging(false);
      return;
    }

    const saved = storedPosition(id);
    if (!saved) {
      setPosition(null);
      return;
    }

    const frame = window.requestAnimationFrame(() => {
      const el = surfaceRef.current;
      if (!el) return;
      const rect = el.getBoundingClientRect();
      const next = clampPosition(saved, { width: rect.width, height: rect.height }, clampMargin);
      positionRef.current = next;
      setPosition(next);
      applyPosition(next);
      if (!positionsMatch(saved, next)) persistPosition(id, next);
    });

    return () => window.cancelAnimationFrame(frame);
  }, [applyPosition, cancelQueuedPosition, clampMargin, enabled, id]);

  useEffect(() => {
    if (!enabled) return;

    const onResize = (): void => {
      const current = positionRef.current;
      const el = surfaceRef.current;
      if (!current || !el) return;
      const rect = el.getBoundingClientRect();
      const next = clampPosition(current, { width: rect.width, height: rect.height }, clampMargin);
      positionRef.current = next;
      setPosition(next);
      applyPosition(next);
      if (!positionsMatch(current, next)) persistPosition(id, next);
    };
    const handleResize = (): void => onResize();

    window.addEventListener('resize', handleResize);
    return () => window.removeEventListener('resize', handleResize);
  }, [applyPosition, clampMargin, enabled, id]);

  useEffect(() => {
    return () => {
      cleanupDragRef.current?.();
      cleanupDragRef.current = null;
      cancelQueuedPosition();
    };
  }, [cancelQueuedPosition]);

  const startDrag = useCallback((event: JSX.TargetedPointerEvent<HTMLElement>): void => {
    if (!enabled || event.button !== 0) return;
    const target = event.target instanceof HTMLElement ? event.target : null;
    if (target?.closest(DRAG_IGNORE_SELECTOR)) return;

    const el = surfaceRef.current;
    if (!el) return;

    cleanupDragRef.current?.();
    cleanupDragRef.current = null;

    const rect = el.getBoundingClientRect();
    const origin = clampPosition({ x: rect.left, y: rect.top }, { width: rect.width, height: rect.height }, clampMargin);
    dragRef.current = {
      startClientX: event.clientX,
      startClientY: event.clientY,
      origin,
      size: { width: rect.width, height: rect.height },
    };
    positionRef.current = origin;
    setPosition(origin);
    applyPosition(origin);
    setIsDragging(true);

    const previousUserSelect = document.body.style.userSelect;
    const previousCursor = document.body.style.cursor;
    document.body.style.userSelect = 'none';
    document.body.style.cursor = 'grabbing';

    const cleanup = (): void => {
      window.removeEventListener('pointermove', onPointerMove);
      window.removeEventListener('pointerup', onPointerUp);
      window.removeEventListener('pointercancel', onPointerCancel);
      cancelQueuedPosition();
      document.body.style.userSelect = previousUserSelect;
      document.body.style.cursor = previousCursor;
      cleanupDragRef.current = null;
    };

    const finish = (): void => {
      cleanup();
      dragRef.current = null;
      setIsDragging(false);
      const current = positionRef.current;
      if (current) {
        applyPosition(current);
        setPosition(current);
        persistPosition(id, current);
      }
    };

    const onPointerMove = (moveEvent: PointerEvent): void => {
      const drag = dragRef.current;
      if (!drag) return;
      const next = clampPosition({
        x: drag.origin.x + moveEvent.clientX - drag.startClientX,
        y: drag.origin.y + moveEvent.clientY - drag.startClientY,
      }, drag.size, clampMargin);
      queuePosition(next);
    };

    const onPointerUp = (): void => finish();
    const onPointerCancel = (): void => finish();

    cleanupDragRef.current = cleanup;
    window.addEventListener('pointermove', onPointerMove);
    window.addEventListener('pointerup', onPointerUp);
    window.addEventListener('pointercancel', onPointerCancel);
    event.preventDefault();
  }, [applyPosition, cancelQueuedPosition, clampMargin, enabled, id, queuePosition]);

  const style = useMemo<JSX.CSSProperties | undefined>(() => {
    if (!position) return undefined;
    return {
      position: 'absolute',
      left: `${Math.round(position.x)}px`,
      top: `${Math.round(position.y)}px`,
    };
  }, [position]);

  return {
    surfaceRef,
    dragHandleProps: { onPointerDown: startDrag },
    isDragging,
    isPositioned: !!position,
    style,
  };
}
