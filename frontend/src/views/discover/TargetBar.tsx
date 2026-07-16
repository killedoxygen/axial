import type { JSX } from 'preact';
import { Icon } from '../../ui/Icons';
import { InstanceTile } from '../../ui/InstanceVisual';
import { navigate, route } from '../../ui-state';
import type { EnrichedInstance } from '../../types-instance';

export function TargetBar({ instance }: { instance: EnrichedInstance }): JSX.Element {
  const browseAll = (): void => {
    const current = route.value;
    if (current.name === 'content') navigate({ name: 'content', id: current.id });
    else navigate({ name: 'discover' });
  };

  const summary = instance.version_display.summary_label;
  const redundant = summary.trim().toLowerCase() === instance.name.trim().toLowerCase();

  return (
    <div class="cp-discover-target" role="status">
      <button
        type="button"
        class="cp-discover-target-who"
        title={`Open ${instance.name}`}
        onClick={() => navigate({ name: 'instance', id: instance.id })}
      >
        <span class="cp-discover-target-tile">
          <InstanceTile inst={instance} radius={999} />
        </span>
        <span class="cp-discover-target-text">
          Adding to <b>{instance.name}</b>
          {!redundant && <span class="cp-discover-target-sub">({summary})</span>}
        </span>
      </button>
      <span class="cp-discover-target-div" aria-hidden="true" />
      <button
        type="button"
        class="cp-discover-target-leave"
        onClick={browseAll}
        title="Stop adding to this instance and browse everything"
      >
        <Icon name="x" size={13} stroke={2.2} />
        Browse all
      </button>
    </div>
  );
}
