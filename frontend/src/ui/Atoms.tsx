import type { JSX, ComponentChildren } from 'preact';
import { useState } from 'preact/hooks';
import type { SoundKind } from '../sound';
import { Icon } from './Icons';

type Tone = 'neutral' | 'accent' | 'ok' | 'warn' | 'err' | 'info';
type BtnVariant = 'primary' | 'secondary' | 'soft' | 'ghost' | 'danger';
type BtnSize = 'sm' | 'md' | 'lg';

export function Button({
  children,
  variant = 'primary',
  size = 'md',
  icon,
  trailing,
  onClick,
  style,
  disabled,
  full,
  title,
  buttonRef,
  sound,
}: {
  children?: ComponentChildren;
  variant?: BtnVariant;
  size?: BtnSize;
  icon?: string;
  trailing?: ComponentChildren;
  onClick?: (e: MouseEvent) => void;
  style?: JSX.CSSProperties;
  disabled?: boolean;
  full?: boolean;
  title?: string;
  buttonRef?: { current: HTMLButtonElement | null };
  sound?: SoundKind | false;
}): JSX.Element {
  const cls = `cp-btn cp-btn--${size} cp-btn--${variant}${full ? ' cp-btn--full' : ''}`;
  return (
    <button
      ref={buttonRef}
      class={cls}
      onClick={disabled ? undefined : onClick}
      disabled={disabled}
      style={style}
      title={title}
      data-sound={sound || undefined}
      data-sound-silent={sound === false ? 'true' : undefined}
    >
      {icon && <Icon name={icon} size={size === 'lg' ? 18 : 16} stroke={1.8} />}
      {children != null && <span>{children}</span>}
      {trailing}
    </button>
  );
}

export function IconButton({
  icon,
  onClick,
  size = 32,
  active,
  style,
  tooltip,
  disabled,
  danger,
  variant,
}: {
  icon: string;
  onClick?: (e: MouseEvent) => void;
  size?: number;
  active?: boolean;
  style?: JSX.CSSProperties;
  tooltip?: string;
  disabled?: boolean;
  danger?: boolean;
  variant?: 'overlay';
}): JSX.Element {
  const inner = Math.round(size * 0.55);
  const cls = `cp-ibtn${active ? ' cp-ibtn--active' : ''}${danger ? ' cp-ibtn--danger' : ''}`;
  const overlay: JSX.CSSProperties = variant === 'overlay' ? { background: 'rgba(0,0,0,0.3)', color: 'white' } : {};
  return (
    <button
      class={cls}
      onClick={disabled ? undefined : onClick}
      title={tooltip}
      disabled={disabled}
      style={{ width: size, height: size, ...overlay, ...style }}
    >
      <Icon name={icon} size={inner} stroke={1.8} />
    </button>
  );
}

export function Pill({
  children,
  tone = 'neutral',
  icon,
  style,
}: {
  children?: ComponentChildren;
  tone?: Tone;
  icon?: string;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const cls = `cp-pill${tone !== 'neutral' ? ` cp-pill--${tone}` : ''}`;
  return (
    <span class={cls} style={style}>
      {icon && <Icon name={icon} size={11} stroke={2} />}
      {children}
    </span>
  );
}

export function Toggle({ on, onChange }: { on: boolean; onChange: () => void }): JSX.Element {
  return <button type="button" class="cp-toggle" data-on={on} role="switch" aria-checked={on} onClick={onChange} />;
}

export function Kbd({ children }: { children: ComponentChildren }): JSX.Element {
  return <span class="cp-kbd">{children}</span>;
}

export function Divider({ vertical, style }: { vertical?: boolean; style?: JSX.CSSProperties }): JSX.Element {
  return (
    <div
      style={{
        background: 'var(--line)',
        width: vertical ? 1 : '100%',
        height: vertical ? '100%' : 1,
        flexShrink: 0,
        ...style,
      }}
    />
  );
}

export function Meter({
  value,
  tone = 'accent',
  height = 4,
  style,
  ariaLabel,
}: {
  value: number;
  tone?: 'accent' | 'ok' | 'warn' | 'err';
  height?: number;
  style?: JSX.CSSProperties;
  ariaLabel?: string;
}): JSX.Element {
  const cls = tone === 'accent' ? 'cp-meter' : `cp-meter cp-meter--${tone}`;
  const finiteValue = Number.isFinite(value) ? value : 0;
  const boundedValue = Math.max(0, Math.min(100, finiteValue));
  return (
    <div
      class={cls}
      style={{ height, ...style }}
      role="progressbar"
      aria-label={ariaLabel}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(boundedValue)}
    >
      <span style={{ width: `${boundedValue}%` }} />
    </div>
  );
}

export function Input({
  value,
  onChange,
  placeholder,
  icon,
  trailing,
  style,
  type = 'text',
  autoFocus,
  onKeyDown,
  onFocus,
  onBlur,
  inputRef,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  icon?: string;
  trailing?: ComponentChildren;
  style?: JSX.CSSProperties;
  type?: string;
  autoFocus?: boolean;
  onKeyDown?: (e: KeyboardEvent) => void;
  onFocus?: () => void;
  onBlur?: () => void;
  inputRef?: { current: HTMLInputElement | null };
}): JSX.Element {
  const [focus, setFocus] = useState(false);
  return (
    <div class={`cp-field${focus ? ' cp-field--focused' : ''}`} style={style}>
      {icon && <Icon name={icon} size={14} color="var(--text-dim)" />}
      <input
        ref={inputRef}
        type={type}
        value={value}
        autocomplete="off"
        spellcheck={false}
        autoFocus={autoFocus}
        onKeyDown={onKeyDown as any}
        onInput={(e: any) => onChange(e.currentTarget.value)}
        onFocus={() => {
          setFocus(true);
          onFocus?.();
        }}
        onBlur={() => {
          setFocus(false);
          onBlur?.();
        }}
        placeholder={placeholder}
      />
      {trailing}
    </div>
  );
}

export function Card({
  children,
  padding = 18,
  style,
  onClick,
  class: cls,
}: {
  children?: ComponentChildren;
  padding?: number;
  style?: JSX.CSSProperties;
  onClick?: (e: MouseEvent) => void;
  class?: string;
}): JSX.Element {
  return (
    <div class={cls ? `cp-card ${cls}` : 'cp-card'} style={{ padding, ...style }} onClick={onClick}>
      {children}
    </div>
  );
}

export function SectionHeading({
  title,
  action,
  right,
}: {
  title?: string;
  action?: { label: string; onClick?: () => void };
  right?: ComponentChildren;
}): JSX.Element {
  return (
    <div class="cp-section-head">
      <div style={{ flex: 1, minWidth: 0 }}>{title && <h2>{title}</h2>}</div>
      {right}
      {action && (
        <button class="cp-section-action" onClick={action.onClick}>
          {action.label} <Icon name="chevron-right" size={13} />
        </button>
      )}
    </div>
  );
}
