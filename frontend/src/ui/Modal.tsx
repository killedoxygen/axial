import type { ComponentChildren, JSX } from 'preact';
import { createContext } from 'preact';
import { createPortal } from 'preact/compat';
import { useContext, useEffect, useRef } from 'preact/hooks';
import { Icon } from './Icons';
import { cn } from '../utils';

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
  return (
    <ModalContext.Provider value={{ close: () => onOpenChange?.(false) }}>
      {children}
    </ModalContext.Provider>
  );
}

const FOCUSABLE = 'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

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

  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const panel = panelRef.current;
    const autofocus = panel?.querySelector<HTMLElement>('[data-autofocus], input, button');
    (autofocus ?? panel)?.focus();

    const onKeyDown = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        close();
        return;
      }
      if (e.key === 'Tab' && panel) {
        const focusable = Array.from(panel.querySelectorAll<HTMLElement>(FOCUSABLE))
          .filter((el) => el.offsetParent !== null || el === document.activeElement);
        if (focusable.length === 0) return;
        const first = focusable[0]!;
        const last = focusable[focusable.length - 1]!;
        const active = document.activeElement;
        if (e.shiftKey && (active === first || active === panel)) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && active === last) {
          e.preventDefault();
          first.focus();
        }
      }
    };
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
      previouslyFocused?.focus?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return createPortal(
    <>
      <div
        data-slot="modal-overlay"
        class="cp-modal-overlay"
        onClick={close}
        aria-hidden="true"
      />
      <div
        data-slot="modal-content"
        role="dialog"
        aria-modal="true"
        tabIndex={-1}
        ref={panelRef}
        class={cn('cp-modal-panel', className)}
        {...props}
      >
        {children}
        {showCloseButton && (
          <button
            type="button"
            data-slot="modal-close"
            class="cp-modal-x"
            aria-label="Close"
            onClick={close}
          >
            <Icon name="x" size={16} stroke={2} />
          </button>
        )}
      </div>
    </>,
    document.body,
  );
}

function ModalClose({
  className,
  children,
  ...props
}: Omit<JSX.HTMLAttributes<HTMLButtonElement>, 'className'> & {
  className?: string;
}): JSX.Element {
  const { close } = useContext(ModalContext);
  return (
    <button
      type="button"
      data-slot="modal-close"
      class={cn(className)}
      onClick={close}
      {...props}
    >
      {children}
    </button>
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

function ModalFooter({
  className,
  children,
  ...props
}: Omit<JSX.HTMLAttributes<HTMLDivElement>, 'className'> & { className?: string }): JSX.Element {
  return (
    <div data-slot="modal-footer" class={cn('cp-modal-foot', className)} {...props}>
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

function ModalDescription({
  className,
  children,
  ...props
}: Omit<JSX.HTMLAttributes<HTMLParagraphElement>, 'className'> & { className?: string }): JSX.Element {
  return (
    <p data-slot="modal-description" class={cn('cp-modal-desc', className)} {...props}>
      {children}
    </p>
  );
}

export {
  Modal,
  ModalClose,
  ModalContent,
  ModalDescription,
  ModalFooter,
  ModalHeader,
  ModalTitle,
};
