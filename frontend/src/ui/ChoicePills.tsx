import type { JSX } from 'preact';
import { useRef } from 'preact/hooks';

export type ChoicePillOption<T extends string> = {
  value: T;
  label: string;
  note?: string;
  disabled?: boolean;
};

export function ChoicePills<T extends string>({
  value,
  options,
  onChange,
  ariaLabel,
  disabled,
}: {
  value: T;
  options: Array<ChoicePillOption<T>>;
  onChange: (v: T) => void;
  ariaLabel: string;
  disabled?: boolean;
}): JSX.Element {
  const groupRef = useRef<HTMLDivElement>(null);

  const blocked = (index: number): boolean => disabled || Boolean(options[index]?.disabled);

  const focusAndSelect = (index: number): void => {
    const option = options[index];
    if (!option || blocked(index)) return;
    groupRef.current?.querySelectorAll('button')[index]?.focus();
    if (option.value !== value) onChange(option.value);
  };

  const step = (from: number, dir: 1 | -1): number => {
    for (let i = 1; i <= options.length; i++) {
      const idx = (from + dir * i + options.length * i) % options.length;
      if (!blocked(idx)) return idx;
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

  const activeIndex = options.findIndex((opt) => opt.value === value);
  const tabStop = activeIndex >= 0 && !blocked(activeIndex) ? activeIndex : options.findIndex((_, i) => !blocked(i));

  return (
    <div ref={groupRef} class="cp-pills" role="radiogroup" aria-label={ariaLabel}>
      {options.map((opt, index) => {
        const active = opt.value === value;
        return (
          <button
            key={opt.value}
            type="button"
            role="radio"
            aria-checked={active}
            data-active={active}
            disabled={blocked(index)}
            title={opt.note}
            tabIndex={index === tabStop ? 0 : -1}
            onClick={() => {
              if (!blocked(index) && !active) onChange(opt.value);
            }}
            onKeyDown={(e) => onKeyDown(e as unknown as KeyboardEvent, index)}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}
