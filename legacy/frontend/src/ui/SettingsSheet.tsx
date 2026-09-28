import type { ComponentChildren, JSX } from 'preact';
import { Pill } from './Atoms';

export function SettingsSection({ title, children }: { title?: string; children: ComponentChildren }): JSX.Element {
  return (
    <section class="cp-sheet-block">
      {title && <h2 class="cp-sheet-heading">{title}</h2>}
      <div class="cp-sheet">{children}</div>
    </section>
  );
}

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
  return (
    <div class={`cp-sheet-row${children ? ' cp-sheet-row--stack' : ''}`}>
      <div class="cp-sheet-row-copy">
        <div class="cp-sheet-row-head">
          <strong>{title}</strong>
          {aside}
        </div>
        {description && <p>{description}</p>}
      </div>
      {control && <div class="cp-sheet-row-control">{control}</div>}
      {children && <div class="cp-sheet-row-body">{children}</div>}
    </div>
  );
}

export function OverrideChip({ onReset, label = 'Overridden' }: { onReset: () => void; label?: string }): JSX.Element {
  return (
    <span class="cp-sheet-override">
      <Pill>{label}</Pill>
      <button type="button" class="cp-sheet-reset" onClick={onReset}>
        Reset
      </button>
    </span>
  );
}
