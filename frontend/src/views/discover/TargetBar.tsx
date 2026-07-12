import type { JSX } from 'preact';
import { Icon } from '../../ui/Icons';
import { InstanceTile } from '../../ui/InstanceVisual';
import { navigate, route } from '../../ui-state';
import type { EnrichedInstance } from '../../types-instance';

/**
 * The standing answer to "where is this going?". Whenever Discover is entered
 * from an instance, this stays on screen: results are filtered to what fits, and
 * every action goes here. Clearing it drops back to plain browsing rather than
 * to some other page.
 */
export function TargetBar({ instance }: { instance: EnrichedInstance }): JSX.Element {
  const display = instance.version_display;

  const browseAll = (): void => {
    const current = route.value;
    if (current.name === 'content') navigate({ name: 'content', id: current.id });
    else navigate({ name: 'discover' });
  };

  return (
    <div class="cp-discover-target" role="status">
      <InstanceTile inst={instance} radius={9} className="cp-discover-target-tile" />
      <div class="cp-discover-target-text">
        <div class="cp-discover-target-label">Adding to</div>
        <div class="cp-discover-target-name" title={instance.name}>
          {instance.name}
        </div>
      </div>
      <span class="cp-discover-target-chip">
        <Icon name="cube" size={12} />
        {display.summary_label}
      </span>
      <div class="cp-discover-target-spacer" />
      <button
        class="cp-discover-target-action"
        onClick={() => navigate({ name: 'instance', id: instance.id })}
        title={`Open ${instance.name}`}
      >
        Open instance
      </button>
      <button class="cp-discover-target-action" onClick={browseAll} title="Browse everything, not just what fits">
        <Icon name="x" size={12} />
        Browse all
      </button>
    </div>
  );
}
