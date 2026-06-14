import type { JSX } from 'preact';
import { createPortal } from 'preact/compat';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Icon } from './Icons';

export interface SelectFieldOption<T extends string = string> {
  value: T;
  label: string;
  disabled?: boolean;
}

interface PopoverPlacement {
  left: number;
  top: number;
  minWidth: number;
  maxHeight: number;
}

function placePopover(trigger: HTMLElement): PopoverPlacement {
  const rect = trigger.getBoundingClientRect();
  const gap = 6;
  const margin = 10;
  const below = window.innerHeight - rect.bottom - gap - margin;
  const above = rect.top - gap - margin;
  const openUp = below < 160 && above > below;
  const maxHeight = Math.min(320, Math.max(120, openUp ? above : below));
  return {
    left: Math.max(margin, Math.min(rect.left, window.innerWidth - rect.width - margin)),
    top: openUp ? Math.max(margin, rect.top - gap - maxHeight) : rect.bottom + gap,
    minWidth: rect.width,
    maxHeight,
  };
}

export function SelectField<T extends string>({
  value,
  onChange,
  options,
  ariaLabel,
  disabled,
  placeholder,
  width,
}: {
  value: T;
  onChange: (value: T) => void;
  options: Array<SelectFieldOption<T>>;
  ariaLabel?: string;
  disabled?: boolean;
  placeholder?: string;
  width?: number | string;
}): JSX.Element {
  const [open, setOpen] = useState(false);
  const [highlighted, setHighlighted] = useState(-1);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const typeahead = useRef<{ buffer: string; timer: number }>({ buffer: '', timer: 0 });

  const selected = options.find((option) => option.value === value);
  const enabledIndexes = useMemo(
    () => options.map((option, index) => (option.disabled ? -1 : index)).filter((index) => index >= 0),
    [options],
  );

  const openList = (): void => {
    if (disabled || options.length === 0) return;
    const selectedIndex = options.findIndex((option) => option.value === value && !option.disabled);
    setHighlighted(selectedIndex >= 0 ? selectedIndex : (enabledIndexes[0] ?? -1));
    setOpen(true);
  };

  const closeList = (focusTrigger = true): void => {
    setOpen(false);
    if (focusTrigger) triggerRef.current?.focus();
  };

  const commit = (index: number): void => {
    const option = options[index];
    if (!option || option.disabled) return;
    onChange(option.value);
    closeList();
  };

  const moveHighlight = (delta: number): void => {
    if (enabledIndexes.length === 0) return;
    const position = enabledIndexes.indexOf(highlighted);
    const next =
      position < 0
        ? delta > 0
          ? 0
          : enabledIndexes.length - 1
        : Math.max(0, Math.min(enabledIndexes.length - 1, position + delta));
    setHighlighted(enabledIndexes[next]!);
  };

  const handleTypeahead = (key: string): void => {
    if (key.length !== 1 || !/\S/.test(key)) return;
    window.clearTimeout(typeahead.current.timer);
    typeahead.current.buffer += key.toLowerCase();
    typeahead.current.timer = window.setTimeout(() => {
      typeahead.current.buffer = '';
    }, 600);
    const match = options.findIndex(
      (option) => !option.disabled && option.label.toLowerCase().startsWith(typeahead.current.buffer),
    );
    if (match >= 0) {
      if (open) setHighlighted(match);
      else onChange(options[match]!.value);
    }
  };

  const onTriggerKeyDown = (e: KeyboardEvent): void => {
    if (disabled) return;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      openList();
      return;
    }
    handleTypeahead(e.key);
  };

  const onListKeyDown = (e: KeyboardEvent): void => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      closeList();
      return;
    }
    if (e.key === 'Tab') {
      closeList(false);
      return;
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      moveHighlight(1);
      return;
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      moveHighlight(-1);
      return;
    }
    if (e.key === 'Home') {
      e.preventDefault();
      setHighlighted(enabledIndexes[0] ?? -1);
      return;
    }
    if (e.key === 'End') {
      e.preventDefault();
      setHighlighted(enabledIndexes[enabledIndexes.length - 1] ?? -1);
      return;
    }
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      commit(highlighted);
      return;
    }
    handleTypeahead(e.key);
  };

  useEffect(() => {
    if (!open) return;
    listRef.current?.focus();
    const onPointerDown = (e: PointerEvent): void => {
      const target = e.target as Node;
      if (listRef.current?.contains(target) || triggerRef.current?.contains(target)) return;
      closeList(false);
    };
    const onWindowChange = (): void => closeList(false);
    document.addEventListener('pointerdown', onPointerDown, true);
    window.addEventListener('resize', onWindowChange);
    window.addEventListener('blur', onWindowChange);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown, true);
      window.removeEventListener('resize', onWindowChange);
      window.removeEventListener('blur', onWindowChange);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  useEffect(() => {
    if (!open || highlighted < 0) return;
    listRef.current?.querySelector<HTMLElement>(`[data-index="${highlighted}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [open, highlighted]);

  const placement = open && triggerRef.current ? placePopover(triggerRef.current) : null;

  return (
    <>
      <button
        type="button"
        ref={triggerRef}
        class="cp-select-trigger"
        role="combobox"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={ariaLabel}
        data-state={open ? 'open' : 'closed'}
        disabled={disabled}
        style={width != null ? { width } : undefined}
        onClick={() => (open ? closeList() : openList())}
        onKeyDown={onTriggerKeyDown}
      >
        <span class="cp-select-value">
          {selected ? selected.label : <span class="cp-select-placeholder">{placeholder ?? 'Select…'}</span>}
        </span>
        <span class="cp-select-caret" aria-hidden="true">
          <Icon name="chevron-down" size={14} stroke={2} />
        </span>
      </button>
      {open &&
        placement &&
        createPortal(
          <div
            ref={listRef}
            class="cp-select-content"
            role="listbox"
            tabIndex={-1}
            aria-label={ariaLabel}
            style={{
              position: 'fixed',
              left: placement.left,
              top: placement.top,
              minWidth: placement.minWidth,
              maxHeight: placement.maxHeight,
            }}
            onKeyDown={onListKeyDown}
          >
            <div class="cp-select-viewport">
              {options.map((option, index) => (
                <div
                  key={option.value}
                  class="cp-select-item"
                  role="option"
                  data-index={index}
                  aria-selected={option.value === value}
                  data-state={option.value === value ? 'checked' : 'unchecked'}
                  data-highlighted={index === highlighted ? '' : undefined}
                  data-disabled={option.disabled ? '' : undefined}
                  onPointerEnter={() => {
                    if (!option.disabled) setHighlighted(index);
                  }}
                  onClick={() => commit(index)}
                >
                  <span class="cp-select-item-label">{option.label}</span>
                  {option.value === value && (
                    <span class="cp-select-check" aria-hidden="true">
                      <Icon name="check" size={13} stroke={2.4} />
                    </span>
                  )}
                </div>
              ))}
            </div>
          </div>,
          document.body,
        )}
    </>
  );
}
