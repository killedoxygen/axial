import type { JSX } from 'preact';
import { createPortal } from 'preact/compat';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';
import { tipProps } from '../../ui/Tip';
import type { CreateDraftItem } from '../../ui-state';
import type { CompatCandidate } from '../../types-content';
import { KIND_ICON } from '../discover/shared';

function PickIcon({ item, missing }: { item: CreateDraftItem; missing: boolean }): JSX.Element {
  return (
    <>
      {item.icon_url ? <img src={item.icon_url} alt="" /> : <Icon name={KIND_ICON[item.kind]} size={12} stroke={2} />}
      {missing && (
        <span class="cp-cr-compat-x" aria-hidden="true">
          <Icon name="x" size={8} stroke={2.5} />
        </span>
      )}
    </>
  );
}

/** Which staged picks a version row can or cannot carry, whichever list is shorter. */
export function CompatibilityPreview({
  candidate,
  draft,
}: {
  candidate: CompatCandidate;
  draft: CreateDraftItem[];
}): JSX.Element | null {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const dropped = new Set(candidate.drops.map((item) => item.canonical_id));
  const working = draft.filter((item) => !dropped.has(item.canonical_id));
  const notWorking = draft.filter((item) => dropped.has(item.canonical_id));
  const showingFits = working.length < notWorking.length;
  const items = showingFits ? working : notWorking;
  const visible = items.slice(0, 3);
  const overflow = items.slice(visible.length);
  const label = showingFits ? 'Works' : "Doesn't work";
  const rect = open ? triggerRef.current?.getBoundingClientRect() : null;
  const popoverTop = rect
    ? rect.bottom + 250 > window.innerHeight
      ? Math.max(8, rect.top - 246)
      : rect.bottom + 6
    : 0;

  useEffect(() => {
    if (!open) return;
    const close = (): void => setOpen(false);
    const closeOutside = (event: PointerEvent): void => {
      const target = event.target as Node;
      if (triggerRef.current?.contains(target) || popoverRef.current?.contains(target)) return;
      close();
    };
    window.addEventListener('resize', close);
    window.addEventListener('scroll', close, true);
    document.addEventListener('pointerdown', closeOutside);
    return () => {
      window.removeEventListener('resize', close);
      window.removeEventListener('scroll', close, true);
      document.removeEventListener('pointerdown', closeOutside);
    };
  }, [open]);

  if (notWorking.length === 0) return null;

  return (
    <span class="cp-cr-compat" aria-label={`${label}: ${items.map((item) => item.title).join(', ')}`}>
      {visible.map((item) => (
        <span
          key={item.canonical_id}
          class="cp-cr-compat-icon"
          aria-label={`${item.title}: ${label.toLowerCase()}`}
          tabIndex={0}
          {...tipProps(`${item.title}: ${label}`)}
        >
          <PickIcon item={item} missing={!showingFits} />
        </span>
      ))}
      {overflow.length > 0 && (
        <button
          ref={triggerRef}
          type="button"
          class="cp-chip cp-cr-compat-more"
          aria-expanded={open}
          title={`Show all ${items.length}`}
          onClick={(event) => {
            event.stopPropagation();
            setOpen((value) => !value);
          }}
        >
          +{overflow.length}
        </button>
      )}
      {open && rect
        ? createPortal(
            <div
              ref={popoverRef}
              class="cp-pop cp-cr-compat-list"
              role="tooltip"
              style={{ top: `${popoverTop}px`, right: `${window.innerWidth - rect.right}px` }}
              onClick={(event) => event.stopPropagation()}
            >
              <strong>{label}</strong>
              {items.map((item) => (
                <span key={item.canonical_id} class="cp-cr-compat-item">
                  <span class="cp-cr-compat-icon">
                    <PickIcon item={item} missing={!showingFits} />
                  </span>
                  {item.title}
                </span>
              ))}
            </div>,
            document.body,
          )
        : null}
    </span>
  );
}
