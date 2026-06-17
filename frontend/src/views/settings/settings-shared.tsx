import type { ComponentChildren, JSX } from 'preact';
import { Card } from '../../ui/Atoms';

export function SettingsCard({
  title,
  desc,
  control,
  stack,
  children,
}: {
  title: string;
  desc?: string;
  control?: ComponentChildren;
  stack?: boolean;
  children?: ComponentChildren;
}): JSX.Element {
  return (
    <Card class={`cp-settings-card${stack ? ' cp-settings-card--stack' : ''}`}>
      <div>
        <div class="cp-settings-card-title">{title}</div>
        {desc && <div class="cp-settings-card-desc">{desc}</div>}
        {stack && children}
      </div>
      {(control || (!stack && children)) && <div class="cp-settings-card-control">{control || children}</div>}
    </Card>
  );
}

export function Toggle({ on, onChange }: { on: boolean; onChange: () => void }): JSX.Element {
  return <button type="button" class="cp-toggle" data-on={on} role="switch" aria-checked={on} onClick={onChange} />;
}
