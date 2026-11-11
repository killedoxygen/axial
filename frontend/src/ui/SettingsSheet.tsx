import { isValidElement, type ComponentChildren, type JSX } from 'preact';
import { useRef } from 'preact/hooks';
import { Chip, Toggle } from './Atoms';
import { Icon } from './Icons';

/** A page of settings: one stack of blocks on the page's left edge. */
export function SettingsSection({ title, children }: { title?: string; children: ComponentChildren }): JSX.Element {
  return (
    <section class="cp-form" aria-label={title}>
      {children}
    </section>
  );
}

/**
 * One setting: its name and the app's line about it, then its control underneath. A switch leads its block with
 * the name beside it.
 */
export function SettingRow({
  title,
  description,
  control,
  children,
  aside,
}: {
  title: string;
  description?: ComponentChildren;
  control?: ComponentChildren;
  children?: ComponentChildren;
  aside?: ComponentChildren;
}): JSX.Element {
  const isSwitch = isValidElement(control) && control.type === Toggle;
  const head = (
    <div class="cp-setting-head">
      <div class="cp-setting-name">
        <h2>{title}</h2>
        {aside}
      </div>
      {description && <p>{description}</p>}
    </div>
  );
  if (isSwitch) {
    return (
      <div class="cp-setting cp-setting--switch">
        {control}
        {head}
      </div>
    );
  }
  return (
    <div class="cp-setting">
      {head}
      {control && <div class="cp-setting-control">{control}</div>}
      {children}
    </div>
  );
}

export function OverrideChip({ onReset, label = 'Overridden' }: { onReset: () => void; label?: string }): JSX.Element {
  return (
    <span class="cp-override">
      <Chip>{label}</Chip>
      <button type="button" class="cp-override-reset" onClick={onReset}>
        Reset
      </button>
    </span>
  );
}

export type OptionTile<T extends string> = { value: T; label: string; note?: string; disabled?: boolean };

/** Arrow keys move the choice through the enabled options, as a radio group does. */
function useRadioKeys<T extends string>(
  options: Array<{ value: T; disabled?: boolean }>,
  value: T,
  onChange: (value: T) => void,
  disabled?: boolean,
): { ref: { current: HTMLDivElement | null }; tabStop: number; onKeyDown: (e: KeyboardEvent, index: number) => void } {
  const ref = useRef<HTMLDivElement | null>(null);
  const blocked = (index: number): boolean => Boolean(disabled || options[index]?.disabled);
  const step = (from: number, dir: 1 | -1): number => {
    for (let i = 1; i <= options.length; i++) {
      const index = (((from + dir * i) % options.length) + options.length) % options.length;
      if (!blocked(index)) return index;
    }
    return from;
  };
  const active = options.findIndex((option) => option.value === value);
  const tabStop = active >= 0 && !blocked(active) ? active : options.findIndex((_, i) => !blocked(i));
  const onKeyDown = (e: KeyboardEvent, index: number): void => {
    let target: number | null = null;
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') target = step(index, 1);
    else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') target = step(index, -1);
    else if (e.key === 'Home') target = step(options.length - 1, 1);
    else if (e.key === 'End') target = step(0, -1);
    if (target === null) return;
    e.preventDefault();
    ref.current?.querySelectorAll<HTMLButtonElement>('[role="radio"]')[target]?.focus();
    if (options[target].value !== value) onChange(options[target].value);
  };
  return { ref, tabStop, onKeyDown };
}

function Mark(): JSX.Element {
  return (
    <span class="cp-check cp-choice-mark" data-selected="true" aria-hidden="true">
      <Icon name="check" size={11} stroke={3} />
    </span>
  );
}

/** A few exclusive choices drawn as tiles across the page: each its name and the app's note on it. */
export function OptionTiles<T extends string>({
  value,
  options,
  onChange,
  ariaLabel,
  disabled,
}: {
  value: T;
  options: Array<OptionTile<T>>;
  onChange: (value: T) => void;
  ariaLabel: string;
  disabled?: boolean;
}): JSX.Element {
  const keys = useRadioKeys(options, value, onChange, disabled);
  return (
    <div ref={keys.ref} class="cp-options" data-count={options.length} role="radiogroup" aria-label={ariaLabel}>
      {options.map((option, index) => {
        const active = option.value === value;
        return (
          <button
            key={option.value}
            type="button"
            role="radio"
            class="cp-option"
            aria-checked={active}
            disabled={disabled || option.disabled}
            tabIndex={index === keys.tabStop ? 0 : -1}
            onClick={() => {
              if (!active) onChange(option.value);
            }}
            onKeyDown={(e) => keys.onKeyDown(e as unknown as KeyboardEvent, index)}
          >
            <strong>{option.label}</strong>
            {option.note && <span>{option.note}</span>}
            {active && <Mark />}
          </button>
        );
      })}
    </div>
  );
}

export type ThemeTile<T extends string> = { value: T; label: string; disabledReason?: string };

/** Theme choices: a small drawing of the shell in each theme's colours, its name under it. */
export function ThemeTiles<T extends string>({
  value,
  options,
  onChange,
  ariaLabel,
}: {
  value: T;
  options: Array<ThemeTile<T>>;
  onChange: (value: T) => void;
  ariaLabel: string;
}): JSX.Element {
  const keys = useRadioKeys(
    options.map((option) => ({ value: option.value, disabled: Boolean(option.disabledReason) })),
    value,
    onChange,
  );
  return (
    <div ref={keys.ref} class="cp-themes" role="radiogroup" aria-label={ariaLabel}>
      {options.map((option, index) => {
        const active = option.value === value;
        return (
          <button
            key={option.value}
            type="button"
            role="radio"
            class="cp-theme"
            aria-checked={active}
            disabled={Boolean(option.disabledReason)}
            tabIndex={index === keys.tabStop ? 0 : -1}
            onClick={() => {
              if (!active) onChange(option.value);
            }}
            onKeyDown={(e) => keys.onKeyDown(e as unknown as KeyboardEvent, index)}
          >
            <span class="cp-theme-art" data-theme={option.value} aria-hidden="true">
              <i>
                <b />
                <b />
                <b />
              </i>
              <i>
                <b />
                <b />
                <b />
              </i>
              {active && <Mark />}
            </span>
            <span class="cp-theme-name">
              {option.label}
              {option.disabledReason && <Chip>{option.disabledReason}</Chip>}
            </span>
          </button>
        );
      })}
    </div>
  );
}
