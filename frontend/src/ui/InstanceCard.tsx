import type { JSX } from 'preact';
import { guardedInstanceHue, InstanceTile } from './InstanceVisual';
import { useTheme } from '../hooks/use-theme';
import { Icon } from './Icons';
import { SelectionCheckbox } from './SelectionActionPill';
import { selectionToggleLabel } from './selection';
import { navigate } from '../ui-state';
import { runningSessions, versionById } from '../store';
import { instanceInstallStatus } from '../instance-install-status';
import type { EnrichedInstance } from '../types-instance';

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
  const theme = useTheme();
  const running = !!runningSessions.value[inst.id];
  const version = versionById(inst.version_id);
  const install = instanceInstallStatus(inst, version);
  const installing = install.installing;
  const installBadge = install.state === 'queued' ? install.queuedItem?.title || install.label : 'Installing';
  const launchAction = inst.launch_action;
  const actionIcon =
    launchAction.primary_action === 'launch'
      ? 'play'
      : launchAction.primary_action === 'install'
        ? 'download'
        : 'alert';
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
      style={{ ['--cp-tile-h' as any]: guardedInstanceHue(inst, theme) }}
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
            <Icon name={actionIcon} size={20} stroke={2} />
          </span>
        )}
      </div>
      <div class="cp-icard-body">
        <div class="cp-icard-name" title={inst.name}>
          {inst.name}
        </div>
        <div class="cp-icard-sub">
          {installing ? `${installBadge} · ` : ''}
          {inst.version_display.summary_label}
        </div>
      </div>
    </div>
  );
}
