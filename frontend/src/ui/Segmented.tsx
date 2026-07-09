import type { JSX } from 'preact';
import { useLayoutEffect, useRef, useState } from 'preact/hooks';
import { Icon } from './Icons';

export type SegmentedOption<T extends string> = {
  value: T;
  label: string;
  icon?: string;
  disabled?: boolean;
  title?: string;
};

const ITEM_ROLE = { radiogroup: 'radio', tablist: 'tab' } as const;

export function Segmented<T extends string>({
  options,
  value,
  onChange,
  size = 'md',
  full,
  ariaLabel,
  role = 'radiogroup',
  class: cls,
}: {
  options: Array<SegmentedOption<T>>;
  value: T;
  onChange: (v: T) => void;
  size?: 'sm' | 'md' | 'lg';
  full?: boolean;
  ariaLabel?: string;
  role?: 'radiogroup' | 'tablist';
  class?: string;
}): JSX.Element {
  const trackRef = useRef<HTMLDivElement>(null);
  const [thumb, setThumb] = useState<{ left: number; width: number } | null>(null);

  const activeIndex = options.findIndex((opt) => opt.value === value);
  const optionsKey = options.map((opt) => `${opt.value}:${opt.label}:${opt.disabled ? 1 : 0}`).join('|');

  useLayoutEffect(() => {
    const track = trackRef.current;
    if (!track) return;
    const measure = (): void => {
      const active = track.querySelector<HTMLButtonElement>('button[data-active="true"]');
      if (!active) {
        setThumb(null);
        return;
      }
      setThumb((prev) => {
        const next = { left: active.offsetLeft, width: active.offsetWidth };
        return prev && prev.left === next.left && prev.width === next.width ? prev : next;
      });
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(track);
    return () => observer.disconnect();
  }, [value, optionsKey, size, full]);

  const focusAndSelect = (index: number): void => {
    const option = options[index];
    if (!option || option.disabled) return;
    trackRef.current?.querySelectorAll('button')[index]?.focus();
    if (option.value !== value) onChange(option.value);
  };

  const step = (from: number, dir: 1 | -1): number => {
    for (let i = 1; i <= options.length; i++) {
      const idx = (from + dir * i + options.length * i) % options.length;
      if (!options[idx]?.disabled) return idx;
    }
    return from;
  };

  const onKeyDown = (e: KeyboardEvent, index: number): void => {
    let target: number | null = null;
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') target = step(index, 1);
    else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') target = step(index, -1);
    else if (e.key === 'Home') target = step(options.length - 1, 1);
    else if (e.key === 'End') target = step(0, -1);
    if (target === null) return;
    e.preventDefault();
    focusAndSelect(target);
  };

  const tabStop =
    activeIndex >= 0 && !options[activeIndex]?.disabled ? activeIndex : options.findIndex((opt) => !opt.disabled);

  const rootCls = ['cp-seg', size !== 'md' ? `cp-seg--${size}` : '', full ? 'cp-seg--full' : '', cls ?? '']
    .filter(Boolean)
    .join(' ');

  return (
    <div ref={trackRef} class={rootCls} role={role} aria-label={ariaLabel}>
      {thumb && (
        <span class="cp-seg-thumb" aria-hidden="true" style={{ translate: `${thumb.left}px 0`, width: thumb.width }} />
      )}
      {options.map((opt, index) => {
        const active = opt.value === value;
        return (
          <button
            key={opt.value}
            type="button"
            role={ITEM_ROLE[role]}
            aria-checked={role === 'radiogroup' ? active : undefined}
            aria-selected={role === 'tablist' ? active : undefined}
            data-active={active}
            disabled={opt.disabled}
            title={opt.title}
            tabIndex={index === tabStop ? 0 : -1}
            onClick={() => {
              if (!opt.disabled && !active) onChange(opt.value);
            }}
            onKeyDown={(e) => onKeyDown(e as unknown as KeyboardEvent, index)}
          >
            {opt.icon && <Icon name={opt.icon} size={15} />}
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}
