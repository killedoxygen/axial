import type { JSX } from 'preact';
import { Icon } from '../../ui/Icons';
import { InstanceFacts } from '../../ui/InstanceFacts';
import { InstanceTile } from '../../ui/InstanceVisual';
import type { EnrichedInstance } from '../../types-instance';
import { navigate, route } from '../../ui-state';

/**
 * The instance content is being installed into. Its name opens it; the cross leaves the mode. `facts` adds the
 * loader and version it fixes, where the page has no refining line to carry them.
 */
export function InstallTarget({ instance, facts }: { instance: EnrichedInstance; facts?: boolean }): JSX.Element {
  const leave = (): void => {
    const current = route.value;
    navigate(current.name === 'content' ? { name: 'content', id: current.id } : { name: 'discover' });
  };
  return (
    <div class="cp-discover-target" role="status">
      Installing into
      <span class="cp-discover-target-pill">
        <button
          type="button"
          title={`Open ${instance.name}`}
          onClick={() => navigate({ name: 'instance', id: instance.id })}
        >
          <InstanceTile inst={instance} />
          {instance.name}
          {facts && <InstanceFacts inst={instance} />}
        </button>
        <button type="button" aria-label="Stop installing into this instance" title="Browse everything" onClick={leave}>
          <Icon name="x" size={12} stroke={2.2} />
        </button>
      </span>
    </div>
  );
}
