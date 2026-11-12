import type { JSX } from 'preact';
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Icon } from './Icons';
import { Popover } from './Popover';

export interface SelectFieldOption<T extends string = string> {
  value: T;
  label: string;
  disabled?: boolean;
}

export function SelectField<T extends string>({
  value,
  onChange,
  options,
  ariaLabel,
  disabled,
  placeholder,
  look = 'field',
}: {
  value: T;
  onChange: (value: T) => void;
  options: Array<SelectFieldOption<T>>;
  ariaLabel?: string;
  disabled?: boolean;
  placeholder?: string;
  /** `quiet`: a small refinement beside others. `ghost`: the same with no fill until hover. */
  look?: 'field' | 'quiet' | 'ghost';
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

  useLayoutEffect(() => {
    if (open) listRef.current?.focus();
  }, [open]);

  useEffect(() => {
    if (!open || highlighted < 0) return;
    listRef.current?.querySelector<HTMLElement>(`[data-index="${highlighted}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [open, highlighted]);

  return (
    <>
      <button
        type="button"
        ref={triggerRef}
        class={look === 'field' ? 'cp-select-trigger' : `cp-select-trigger cp-select-trigger--${look}`}
        role="combobox"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={ariaLabel}
        data-state={open ? 'open' : 'closed'}
        disabled={disabled}
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
      {open && (
        <Popover
          anchor={triggerRef}
          popoverRef={listRef}
          onDismiss={() => closeList(false)}
          fit="min-anchor"
          class="cp-pop cp-select-content"
          role="listbox"
          tabIndex={-1}
          aria-label={ariaLabel}
          onKeyDown={onListKeyDown}
        >
          <div class="cp-select-viewport cp-menu">
            {options.map((option, index) => (
              <div
                key={option.value}
                class="cp-select-item cp-menu-item"
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
              </div>
            ))}
          </div>
        </Popover>
      )}
    </>
  );
}
