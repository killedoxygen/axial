import type { ComponentChildren, JSX } from 'preact';
import { createContext } from 'preact';
import { createPortal } from 'preact/compat';
import { useContext, useEffect, useRef } from 'preact/hooks';
import { leaveBehind } from './motion';
import { Icon } from './Icons';
import { cn } from './cn';
import { dialogOpen } from './Dialog';

interface ModalContextValue {
  close: () => void;
}

const ModalContext = createContext<ModalContextValue>({ close: () => undefined });

function Modal({
  open,
  onOpenChange,
  children,
}: {
  open: boolean;
  onOpenChange?: (open: boolean) => void;
  children: ComponentChildren;
}): JSX.Element | null {
  if (!open) return null;
  return <ModalContext.Provider value={{ close: () => onOpenChange?.(false) }}>{children}</ModalContext.Provider>;
}

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function ModalContent({
  className,
  children,
  showCloseButton = true,
  ...props
}: Omit<JSX.HTMLAttributes<HTMLDivElement>, 'className'> & {
  className?: string;
  showCloseButton?: boolean;
}): JSX.Element {
  const { close } = useContext(ModalContext);
  const panelRef = useRef<HTMLDivElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const suspended = dialogOpen.value;

  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const panel = panelRef.current;
    const autofocus =
      panel?.querySelector<HTMLElement>('[data-autofocus]') ?? panel?.querySelector<HTMLElement>('input, button');
    if (!dialogOpen.value) (autofocus ?? panel)?.focus();

    const onKeyDown = (e: KeyboardEvent): void => {
      if (e.defaultPrevented || dialogOpen.value) return;
      if (e.key === 'Escape') {
        e.stopPropagation();
        close();
        return;
      }
      if (e.key === 'Tab' && panel) {
        // Notices offering an action stay reachable from the dialog: the panel and they take turns in one cycle.
        const groups = [panel, ...document.querySelectorAll<HTMLElement>('[data-modal-reachable]')]
          .map((scope) => ({
            scope,
            items: Array.from(scope.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
              (el) => !el.closest('[data-leaving]') && (el.offsetParent !== null || el === document.activeElement),
            ),
          }))
          .filter((group) => group.items.length > 0);
        if (groups.length === 0) return;
        const active = document.activeElement;
        const at = groups.findIndex((group) => group.scope.contains(active));
        const step = (to: number): void => {
          e.preventDefault();
          const { items } = groups[(to + groups.length) % groups.length]!;
          (e.shiftKey ? items[items.length - 1]! : items[0]!).focus();
        };
        if (at === -1) step(e.shiftKey ? -1 : 0);
        else if (e.shiftKey && (active === panel || active === groups[at]!.items[0])) step(at - 1);
        else if (!e.shiftKey && active === groups[at]!.items[groups[at]!.items.length - 1]) step(at + 1);
      }
    };
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
      if (!dialogOpen.value) previouslyFocused?.focus?.();
      // Both leave after they are gone.
      leaveBehind(overlayRef.current);
      leaveBehind(panelRef.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return createPortal(
    <>
      <div
        data-slot="modal-overlay"
        ref={overlayRef}
        class="cp-modal-overlay"
        data-window-drag-strip
        onClick={suspended ? undefined : close}
        inert={suspended}
        aria-hidden="true"
      />
      <div
        data-slot="modal-content"
        role="dialog"
        tabIndex={-1}
        ref={panelRef}
        class={cn('cp-modal-panel', className)}
        {...props}
        aria-modal={suspended ? undefined : 'true'}
        aria-hidden={suspended ? 'true' : undefined}
        inert={suspended}
      >
        {children}
        {showCloseButton && (
          <button type="button" data-slot="modal-close" class="cp-modal-x cp-ibtn" aria-label="Close" onClick={close}>
            <Icon name="x" size={16} stroke={2} />
          </button>
        )}
      </div>
    </>,
    document.body,
  );
}

function ModalHeader({
  className,
  children,
  ...props
}: Omit<JSX.HTMLAttributes<HTMLDivElement>, 'className'> & { className?: string }): JSX.Element {
  return (
    <div data-slot="modal-header" class={cn('cp-modal-head', className)} {...props}>
      {children}
    </div>
  );
}

function ModalTitle({
  className,
  children,
  ...props
}: Omit<JSX.HTMLAttributes<HTMLHeadingElement>, 'className'> & { className?: string }): JSX.Element {
  return (
    <h2 data-slot="modal-title" class={cn('cp-modal-title', className)} {...props}>
      {children}
    </h2>
  );
}

export { Modal, ModalContent, ModalHeader, ModalTitle };
