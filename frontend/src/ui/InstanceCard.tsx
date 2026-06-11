import type { JSX } from 'preact';
import { InstanceTile } from './InstanceVisual';
import { Icon } from './Icons';
import { navigate } from '../ui-state';
import { runningSessions, versions } from '../store';
import { minecraftVersionLabel } from '../version-display';
import { loaderKeyFromVersion, LOADER_LABELS } from '../views/create/defaults';
import type { EnrichedInstance } from '../types';

function versionLabel(inst: EnrichedInstance): { loader: string; mc: string } {
  const v = versions.value.find(x => x.id === inst.version_id);
  const mc = minecraftVersionLabel(v, '—');
  return { loader: LOADER_LABELS[loaderKeyFromVersion(v)], mc };
}

/* Cover card: square art on top, identity below, play overlay on hover.
 * The shared library tile for Home and Instances. */
export function InstanceCard({ inst, onContextMenu }: {
  inst: EnrichedInstance;
  onContextMenu?: (e: MouseEvent) => void;
}): JSX.Element {
  const running = !!runningSessions.value[inst.id];
  const { loader, mc } = versionLabel(inst);
  const open = (): void => navigate({ name: 'instance', id: inst.id });
  const onKeyDown = (e: KeyboardEvent): void => {
    if (e.target !== e.currentTarget) return;
    if (e.key !== 'Enter' && e.key !== ' ') return;
    e.preventDefault();
    open();
  };
  return (
    <div
      class="cp-icard"
      role="button"
      tabIndex={0}
      aria-label={`Open ${inst.name}`}
      data-running={running}
      onClick={open}
      onKeyDown={onKeyDown}
      onContextMenu={onContextMenu}
    >
      <div class="cp-icard-art">
        <InstanceTile inst={inst} radius={0} className="cp-icard-canvas" />
        {running && <span class="cp-icard-live" aria-label="Running"><span /> Live</span>}
        <span class="cp-icard-play" aria-hidden="true">
          <Icon name="play" size={20} stroke={2} />
        </span>
      </div>
      <div class="cp-icard-body">
        <div class="cp-icard-name" title={inst.name}>{inst.name}</div>
        <div class="cp-icard-sub">{loader} · {mc}</div>
      </div>
    </div>
  );
}
