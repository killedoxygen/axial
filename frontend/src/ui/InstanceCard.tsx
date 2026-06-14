import type { JSX } from 'preact';
import { InstanceTile } from './InstanceVisual';
import { Icon } from './Icons';
import { SelectionCheckbox } from './SelectionActionPill';
import { selectionToggleLabel } from './selection';
import { navigate } from '../ui-state';
import { runningSessions, versionById } from '../store';
import { instanceInstallStatus } from '../instance-install-status';
import { minecraftVersionLabel } from '../version-display';
import { loaderKeyFromVersion, LOADER_LABELS } from '../views/create/defaults';
import type { EnrichedInstance } from '../types';

function versionLabel(inst: EnrichedInstance): { loader: string; mc: string } {
  const v = versionById(inst.version_id);
  const mc = minecraftVersionLabel(v, '—');
  return { loader: LOADER_LABELS[loaderKeyFromVersion(v)], mc };
}

/* Cover card: square art on top, identity below, play overlay on hover.
 * The shared library tile for Home and Instances. */
export function InstanceCard({
  inst,
  onContextMenu,
  selected,
  onToggleSelect,
}: {
  inst: EnrichedInstance;
  onContextMenu?: (e: MouseEvent) => void;
  selected?: boolean;
  onToggleSelect?: (e: MouseEvent) => void;
}): JSX.Element {
  const running = !!runningSessions.value[inst.id];
  const version = versionById(inst.version_id);
  const { loader, mc } = versionLabel(inst);
  const install = instanceInstallStatus(inst, version);
  const installing = install.installing;
  const installBadge = install.state === 'queued' ? 'Queued' : 'Installing';
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
      aria-label={installing ? `Open ${inst.name}. ${installBadge}` : `Open ${inst.name}`}
      data-running={running}
      data-installing={installing}
      data-selected={selected === true}
      onClick={open}
      onKeyDown={onKeyDown}
      onContextMenu={onContextMenu}
    >
      <div class="cp-icard-art">
        <InstanceTile inst={inst} radius={0} className="cp-icard-canvas" />
        {onToggleSelect && (
          <SelectionCheckbox
            className="cp-icard-select"
            selected={selected === true}
            label={selectionToggleLabel(selected === true, inst.name)}
            onToggle={(e) => {
              e.stopPropagation();
              onToggleSelect(e);
            }}
          />
        )}
        {running && (
          <span class="cp-icard-live" aria-label="Running">
            <span /> Live
          </span>
        )}
        {installing && (
          <span class="cp-icard-install" aria-label={installBadge}>
            <Icon name={install.state === 'queued' ? 'clock' : 'download'} size={13} stroke={2} />
            {installBadge}
          </span>
        )}
        {!installing && (
          <span class="cp-icard-play" aria-hidden="true">
            <Icon name="play" size={20} stroke={2} />
          </span>
        )}
      </div>
      <div class="cp-icard-body">
        <div class="cp-icard-name" title={inst.name}>
          {inst.name}
        </div>
        <div class="cp-icard-sub">
          {installing ? `${installBadge} · ` : ''}
          {loader} · {mc}
        </div>
      </div>
    </div>
  );
}
