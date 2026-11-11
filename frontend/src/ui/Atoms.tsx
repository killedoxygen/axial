import type { JSX, ComponentChildren } from 'preact';
import type { SoundKind } from '../sound';
import { Face, type Eyes, type FaceTone } from './Face';
import { Icon, type IconName } from './Icons';

/** `ok` and `info` arrive from wire view models; both read as the accent. */
type ChipTone = 'neutral' | 'accent' | 'ok' | 'info' | 'warn' | 'err' | 'bare';
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
  pressed,
  busy,
  type = 'button',
}: {
  children?: ComponentChildren;
  variant?: BtnVariant;
  size?: BtnSize;
  icon?: IconName;
  trailing?: ComponentChildren;
  onClick?: (e: MouseEvent) => void;
  style?: JSX.CSSProperties;
  disabled?: boolean;
  full?: boolean;
  title?: string;
  buttonRef?: { current: HTMLButtonElement | null };
  sound?: SoundKind | false;
  pressed?: boolean;
  /** Still working: the ring stands in for the icon. */
  busy?: boolean;
  type?: 'button' | 'submit';
}): JSX.Element {
  const cls = `cp-btn cp-btn--${size} cp-btn--${variant}${full ? ' cp-btn--full' : ''}`;
  return (
    <button
      ref={buttonRef}
      type={type}
      class={cls}
      onClick={disabled ? undefined : onClick}
      disabled={disabled}
      style={style}
      title={title}
      aria-pressed={pressed}
      data-sound={sound || undefined}
      data-sound-silent={sound === false ? 'true' : undefined}
    >
      {busy ? <Ring /> : icon && <Icon name={icon} size={16} stroke={1.8} />}
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
  icon: IconName;
  onClick?: (e: MouseEvent) => void;
  size?: number;
  active?: boolean;
  style?: JSX.CSSProperties;
  tooltip?: string;
  disabled?: boolean;
  danger?: boolean;
  variant?: 'overlay';
}): JSX.Element {
  const inner = Math.round(size / 2);
  const cls = `cp-ibtn${active ? ' cp-ibtn--active' : ''}${danger ? ' cp-ibtn--danger' : ''}${variant ? ` cp-ibtn--${variant}` : ''}`;
  return (
    <button
      class={cls}
      onClick={disabled ? undefined : onClick}
      title={tooltip}
      aria-label={tooltip}
      disabled={disabled}
      style={{ width: size, height: size, ...style }}
    >
      <Icon name={icon} size={inner} stroke={1.8} />
    </button>
  );
}

/** A fact, never a status. */
export function Chip({
  children,
  tone = 'neutral',
  icon,
  mono,
  title,
}: {
  children?: ComponentChildren;
  tone?: ChipTone;
  icon?: IconName;
  mono?: boolean;
  title?: string;
}): JSX.Element {
  const look = tone === 'ok' || tone === 'info' ? 'accent' : tone;
  const cls = `cp-chip${look !== 'neutral' ? ` cp-chip--${look}` : ''}${mono ? ' cp-chip--mono' : ''}`;
  return (
    <span class={cls} title={title}>
      {icon && <Icon name={icon} size={11} stroke={2} />}
      {children}
    </span>
  );
}

const RING =
  'M8 1.75H10.1A4.15 4.15 0 0 1 14.25 5.9V10.1A4.15 4.15 0 0 1 10.1 14.25H5.9A4.15 4.15 0 0 1 1.75 10.1V5.9A4.15 4.15 0 0 1 5.9 1.75Z';

/** Still going. With `progress` (0 to 100) the stroke runs clockwise from the top; without it a short stroke travels round. */
export function Ring({ progress, size = 16 }: { progress?: number | null; size?: number }): JSX.Element {
  const busy = progress == null || !Number.isFinite(progress);
  return (
    <svg
      class="cp-ring"
      data-busy={busy || undefined}
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      stroke-width={size >= 20 ? 1.9 : 2.2}
      stroke-linecap="round"
      aria-hidden="true"
    >
      <path class="cp-ring-track" d={RING} />
      <path
        class="cp-ring-run"
        d={RING}
        pathLength={100}
        stroke-dasharray={busy ? '26 74' : `${Math.max(0, Math.min(100, progress))} 100`}
      />
    </svg>
  );
}

/** A word with the face (how it ended) or the ring (still going) beside it. */
export function Status({
  children,
  eyes,
  busy,
  tone,
}: {
  children: ComponentChildren;
  eyes?: Eyes;
  busy?: boolean;
  tone?: 'accent' | 'warn' | 'err' | 'mute';
}): JSX.Element {
  return (
    <span class="cp-status" data-tone={tone}>
      {busy ? <Ring size={14} /> : eyes && <Face eyes={eyes} />}
      {children}
    </span>
  );
}

export type NoticeTone = 'info' | 'warned' | 'intervened' | 'success' | 'error';

const NOTICE_FACE: Record<NoticeTone, { eyes: Eyes; tone?: FaceTone }> = {
  info: { eyes: 'open' },
  warned: { eyes: 'wide', tone: 'warn' },
  intervened: { eyes: 'happy' },
  success: { eyes: 'happy' },
  error: { eyes: 'x', tone: 'err' },
};

export function Notice({
  tone = 'info',
  title,
  children,
  onDismiss,
  role,
  class: cls,
}: {
  tone?: NoticeTone;
  title?: ComponentChildren;
  children?: ComponentChildren;
  onDismiss?: () => void;
  role?: 'alert' | 'status';
  class?: string;
}): JSX.Element {
  return (
    <section
      class={cls ? `cp-notice ${cls}` : 'cp-notice'}
      data-tone={tone}
      role={role}
      aria-live={role ? undefined : 'polite'}
    >
      <Face {...NOTICE_FACE[tone]} />
      <div class="cp-notice-copy">
        {title != null && <strong>{title}</strong>}
        {children}
      </div>
      {onDismiss && <IconButton icon="x" size={28} tooltip="Dismiss" onClick={onDismiss} />}
    </section>
  );
}

/** Nothing here: the mascot asleep, a line of what would be here, and the way to add it. `eyes="x"` when it broke. */
export function Empty({
  title,
  children,
  actions,
  eyes = 'closed',
}: {
  title: string;
  children?: ComponentChildren;
  actions?: ComponentChildren;
  eyes?: 'closed' | 'x';
}): JSX.Element {
  return (
    <div class="cp-empty">
      <Face eyes={eyes} size={84} tone={eyes === 'x' ? 'err' : 'mute'} />
      <h2>{title}</h2>
      {children != null && <p>{children}</p>}
      {actions != null && <div class="cp-empty-actions">{actions}</div>}
    </div>
  );
}

export function Toggle({
  on,
  onChange,
  disabled,
  ariaLabel,
}: {
  on: boolean;
  onChange: () => void;
  disabled?: boolean;
  ariaLabel?: string;
}): JSX.Element {
  return (
    <button
      type="button"
      class="cp-toggle"
      data-on={on}
      role="switch"
      aria-checked={on}
      aria-label={ariaLabel}
      disabled={disabled}
      onClick={onChange}
    />
  );
}

export function Kbd({ children }: { children: ComponentChildren }): JSX.Element {
  const oneGlyph = typeof children === 'string' && [...children].length === 1;
  return <span class={oneGlyph ? 'cp-kbd cp-kbd--key' : 'cp-kbd'}>{children}</span>;
}

/** A key combination, one cap per key. */
export function Keys({ parts }: { parts: readonly string[] }): JSX.Element {
  return (
    <span class="cp-keys">
      {parts.map((part) => (
        <Kbd key={part}>{part}</Kbd>
      ))}
    </span>
  );
}

/** What the keys do, under a list that is walked with them. */
export function KeyLegend({
  keys,
}: {
  keys: ReadonlyArray<readonly [caps: readonly string[], does: string]>;
}): JSX.Element {
  return (
    <div class="cp-legend" aria-hidden="true">
      {keys.map(([caps, does]) => (
        <span key={does}>
          <Keys parts={caps} />
          {does}
        </span>
      ))}
    </div>
  );
}

export interface TabItem<T extends string> {
  id: T;
  label: string;
  icon: IconName;
  count?: number | null;
  disabled?: boolean;
  /** Why it is disabled, or more than the label says. */
  title?: string;
}

export function Tabs<T extends string>({
  tabs,
  active,
  onSelect,
  label,
  lead,
}: {
  tabs: ReadonlyArray<TabItem<T>>;
  active: T;
  onSelect: (id: T) => void;
  label?: string;
  /** The page's first line, where a title would stand. */
  lead?: boolean;
}): JSX.Element {
  return (
    <div class={lead ? 'cp-tabs cp-tabs--lead' : 'cp-tabs'} role="tablist" aria-label={label}>
      {tabs.map((tab) => (
        <button
          key={tab.id}
          type="button"
          role="tab"
          aria-selected={active === tab.id}
          data-active={active === tab.id}
          title={tab.title ?? tab.label}
          disabled={tab.disabled}
          onClick={() => onSelect(tab.id)}
        >
          <span class="cp-tab-icon">
            <Icon name={tab.icon} size={15} stroke={1.8} />
          </span>
          <span class="cp-tab-label">{tab.label}</span>
          {tab.count != null && <span class="cp-tab-count">{tab.count}</span>}
        </button>
      ))}
    </div>
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
  tone?: 'accent' | 'warn' | 'err';
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
  invalid,
  ariaLabel,
  lead,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  icon?: IconName;
  trailing?: ComponentChildren;
  style?: JSX.CSSProperties;
  type?: string;
  autoFocus?: boolean;
  onKeyDown?: (e: KeyboardEvent) => void;
  onFocus?: () => void;
  onBlur?: () => void;
  inputRef?: { current: HTMLInputElement | null };
  invalid?: boolean;
  ariaLabel?: string;
  /** The page's main field: one step taller. */
  lead?: boolean;
}): JSX.Element {
  return (
    <div class={lead ? 'cp-field cp-field--lead' : 'cp-field'} style={style} aria-invalid={invalid || undefined}>
      {invalid ? <Face eyes="x" tone="err" /> : icon && <Icon name={icon} size={15} />}
      <input
        ref={inputRef}
        type={type}
        value={value}
        autocomplete="off"
        spellcheck={false}
        autoFocus={autoFocus}
        onKeyDown={onKeyDown as any}
        onInput={(e: any) => onChange(e.currentTarget.value)}
        onFocus={onFocus}
        onBlur={onBlur}
        placeholder={placeholder}
        aria-label={ariaLabel}
      />
      {trailing}
    </div>
  );
}
