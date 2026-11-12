/* Motion that CSS alone cannot do, as plain DOM so any component can use it. The rules of the motion itself
   (curves, times, where things enter from) stay in `base.css`. */

/**
 * Keeps a thumb under the active item of its track, so a choice glides from one item to the next instead of
 * blinking. Given as the thumb's `ref`: it runs after every render, and again whenever the track changes size.
 */
export function glideThumb(thumb: HTMLElement | null): (() => void) | undefined {
  const track = thumb?.parentElement;
  if (!thumb || !track) return;
  const place = (): void => {
    const active = track.querySelector<HTMLElement>(':scope > [data-active="true"]');
    if (!active) {
      thumb.style.opacity = '0';
      return;
    }
    // The first placement, and one after it was hidden, must not travel from nowhere.
    const jump = thumb.style.opacity !== '1';
    if (jump) thumb.style.transition = 'none';
    thumb.style.opacity = '1';
    thumb.style.translate = `${active.offsetLeft}px ${active.offsetTop}px`;
    thumb.style.width = `${active.offsetWidth}px`;
    thumb.style.height = `${active.offsetHeight}px`;
    if (jump) {
      void thumb.offsetWidth;
      thumb.style.transition = '';
    }
  };
  place();
  const observer = new ResizeObserver(place);
  observer.observe(track);
  return () => observer.disconnect();
}

const reduced = (): boolean => matchMedia('(prefers-reduced-motion: reduce)').matches;

/** A motion token from `base.css`, so scripted motion keeps the stylesheet's times and curves. */
function token(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/** A copy of an element that looks as it did: what canvases showed, what fields held, how far lists were scrolled. */
function copyOf(element: HTMLElement): { copy: HTMLElement; restoreScroll: () => void } {
  const copy = element.cloneNode(true) as HTMLElement;
  const live = [...element.querySelectorAll<HTMLElement>('canvas, input, textarea, [data-scrolls]')];
  const copies = copy.querySelectorAll<HTMLElement>('canvas, input, textarea, [data-scrolls]');
  live.forEach((source, index) => {
    const twin = copies[index];
    if (source instanceof HTMLCanvasElement && twin instanceof HTMLCanvasElement) {
      try {
        twin.getContext('2d')?.drawImage(source, 0, 0);
      } catch {
        // A canvas that cannot be read leaves blank.
      }
    } else if (twin && 'value' in source && 'value' in twin) {
      twin.value = source.value;
    }
  });
  copy.inert = true;
  copy.setAttribute('aria-hidden', 'true');
  copy.removeAttribute('id');
  // Scroll offsets only hold once the copy is in the document.
  const restoreScroll = (): void =>
    live.forEach((source, index) => {
      if (source.scrollTop > 0 && copies[index]) copies[index].scrollTop = source.scrollTop;
    });
  return { copy, restoreScroll };
}

/** Lets a ghost leave by the stylesheet's `[data-leaving]` rule, then removes it and closes the gap it held. */
function leave(ghost: HTMLElement): void {
  ghost.dataset.leaving = 'true';
  const remove = (): void => {
    const parent = ghost.parentElement;
    ghost.remove();
    settleChildren(parent);
  };
  ghost.addEventListener('animationend', (event) => event.target === ghost && remove());
  // If the stylesheet gave it no exit, it still goes.
  window.setTimeout(remove, 400);
}

/**
 * Plays a surface's exit after it has been unmounted: an inert copy stands where it was, leaves, and removes
 * itself. Nothing has to stay mounted to animate out.
 */
export function leaveBehind(element: HTMLElement | null): void {
  if (!element?.isConnected || reduced()) return;
  const { copy, restoreScroll } = copyOf(element);
  element.after(copy);
  restoreScroll();
  leave(copy);
}

/** A page larger than this changes at once: copying it would cost more than the transition is worth. */
const VIEW_COPY_LIMIT = 3000;

/**
 * The same for a whole page. `route` wraps the page inside the scrolling view; its copy is laid over the view at
 * the scroll offset it had and fades, while the next page arrives underneath.
 */
export function leaveView(route: HTMLElement | null): void {
  const view = route?.parentElement;
  const page = route?.firstElementChild;
  if (!view || !(page instanceof HTMLElement) || reduced()) return;
  if (page.getElementsByTagName('*').length > VIEW_COPY_LIMIT) return;
  const { copy, restoreScroll } = copyOf(page);
  copy.style.translate = `0 ${-view.scrollTop}px`;
  const ghost = document.createElement('div');
  ghost.className = 'cp-view-ghost';
  ghost.style.cssText = `top:${view.offsetTop}px;left:${view.offsetLeft}px;width:${view.clientWidth}px;height:${view.clientHeight}px`;
  ghost.append(copy);
  view.after(ghost);
  restoreScroll();
  leave(ghost);
}

/** Where layout put an element in the document: unmoved by scrolling and by transforms still running. */
function placeInDocument(element: HTMLElement): { x: number; y: number } {
  let x = 0;
  let y = 0;
  for (let node: Element | null = element; node instanceof HTMLElement; node = node.offsetParent) {
    x += node.offsetLeft;
    y += node.offsetTop;
  }
  return { x, y };
}

const seats = new WeakMap<HTMLElement, { width: number; at: WeakMap<Element, { x: number; y: number }> }>();
/** More rows than this moving at once is a new list, not a rearrangement. */
const SETTLE_LIMIT = 60;

/**
 * Lets the items of a list glide to their new places when it is reordered or one of them goes, instead of
 * jumping. Given as the list's `ref`: it runs after every render, compares each child's place with the last one,
 * and moves by transform only. A list that changed width is being laid out again, not rearranged.
 */
export function settleChildren(list: HTMLElement | null): void {
  if (!list) return;
  const before = seats.get(list);
  // Called for a parent that never asked to be settled (a ghost's removal): nothing to do.
  if (!before && !list.hasAttribute('data-settles')) return;
  const at = new WeakMap<Element, { x: number; y: number }>();
  const moved: [HTMLElement, number, number][] = [];
  const origin = placeInDocument(list);
  for (const child of list.children) {
    if (!(child instanceof HTMLElement)) continue;
    const now =
      child.offsetParent === list
        ? { x: origin.x + child.offsetLeft, y: origin.y + child.offsetTop }
        : placeInDocument(child);
    at.set(child, now);
    const was = before?.at.get(child);
    if (was && (was.x !== now.x || was.y !== now.y)) moved.push([child, was.x - now.x, was.y - now.y]);
  }
  const width = list.offsetWidth;
  seats.set(list, { width, at });
  if (!before || before.width !== width || moved.length === 0 || moved.length > SETTLE_LIMIT || reduced()) return;
  const timing = { duration: parseFloat(token('--t-move')), easing: token('--ease-glide') };
  for (const [child, x, y] of moved) child.animate([{ translate: `${x}px ${y}px` }, { translate: '0 0' }], timing);
}

const widths = new WeakMap<HTMLElement, { width: number; label: string; running?: Animation }>();

/**
 * Lets a control whose words change grow or shrink to them instead of snapping. Given as its `ref`. It starts
 * from the width last on screen, so a change that arrives mid-way carries on from there; small changes, such as
 * a figure ticking over, are left alone.
 */
export function settleWidth(control: HTMLElement | null): void {
  if (!control) return;
  const was = widths.get(control);
  // Mid-way, the width on screen is the animated one; at rest it is the one last laid out.
  const shown = was?.running ? control.offsetWidth : was?.width;
  was?.running?.cancel();
  const record: { width: number; label: string; running?: Animation } = {
    width: control.offsetWidth,
    label: control.textContent ?? '',
  };
  widths.set(control, record);
  if (!was || shown == null || Math.abs(shown - record.width) < 8 || reduced()) return;
  if (was.label === record.label && !was.running) return;
  record.running = control.animate([{ width: `${shown}px` }, { width: `${record.width}px` }], {
    duration: parseFloat(token('--t-move')),
    easing: token('--ease-glide'),
  });
  record.running.onfinish = () => {
    record.running = undefined;
  };
}
