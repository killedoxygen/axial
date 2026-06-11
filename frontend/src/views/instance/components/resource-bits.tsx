import type { JSX } from 'preact';
import { Icon } from '../../../ui/Icons';
import { Button } from '../../../ui/Atoms';
import type { ResourceLoadState } from '../resources';

export function ResourceStatus({
  state,
  onRetry,
}: {
  state: ResourceLoadState;
  onRetry: () => void;
}): JSX.Element | null {
  if (state.status === 'loading' && !state.data) {
    return <div class="cp-resource-note">Loading files…</div>;
  }
  if (state.status === 'error') {
    return (
      <div class="cp-resource-note cp-resource-note--error">
        <span>{state.error}</span>
        <Button variant="secondary" size="sm" icon="refresh" onClick={onRetry}>Retry</Button>
      </div>
    );
  }
  return null;
}

export function ResourceToolbar({
  title,
  onRefresh,
  action,
}: {
  title: string;
  onRefresh: () => void;
  action: { icon: string; label: string; onClick: () => void };
}): JSX.Element {
  return (
    <div class="cp-resource-toolbar">
      <strong>{title}</strong>
      <div>
        <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>Refresh</Button>
        <Button variant="soft" size="sm" icon={action.icon} onClick={action.onClick}>{action.label}</Button>
      </div>
    </div>
  );
}

export function ResourceEmpty({ icon, title, hint }: { icon: string; title: string; hint: string }): JSX.Element {
  return (
    <div class="cp-resource-empty">
      <span><Icon name={icon} size={20} /></span>
      <strong>{title}</strong>
      <p>{hint}</p>
    </div>
  );
}

export function ResourceRow({
  icon,
  name,
  meta,
  actions,
  onContextMenu,
}: {
  icon: string;
  name: string;
  meta: string;
  actions?: JSX.Element;
  onContextMenu?: (e: MouseEvent) => void;
}): JSX.Element {
  return (
    <div class="cp-resource-row" onContextMenu={onContextMenu}>
      <span class="cp-resource-row-icon"><Icon name={icon} size={15} /></span>
      <span class="cp-resource-name" title={name}>{name}</span>
      <span class="cp-resource-meta">{meta}</span>
      {actions ? <span class="cp-resource-actions">{actions}</span> : null}
    </div>
  );
}
