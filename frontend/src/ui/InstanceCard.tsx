import type { JSX } from 'preact';
import { InstanceFacts, SessionChip } from './InstanceFacts';
import { InstanceTile } from './InstanceVisual';
import { Chip } from './Atoms';
import { Icon } from './Icons';
import { SelectionCheckbox } from './SelectionActionTray';
import { selectionToggleLabel } from './selection';
import { navigate } from '../ui-state';
import { launchSessions, versionById } from '../store';
import { instanceInstallStatus } from '../instance-install-status';
import type { EnrichedInstance } from '../types-instance';

/** Opens the instance on Enter or Space when the row or cover itself has focus, not a control inside it. */
export function openOnKey(e: KeyboardEvent, open: () => void): void {
  if (e.target !== e.currentTarget || (e.key !== 'Enter' && e.key !== ' ')) return;
  e.preventDefault();
  open();
}

/** A library cover: the instance's tile, its name, and its facts as chips. */
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
  const session = launchSessions.value[inst.id];
  const install = instanceInstallStatus(inst, versionById(inst.version_id));
  const installLabel = install.state === 'queued' ? install.queuedItem?.title || install.label : 'Installing';
  const action = inst.launch_action.primary_action;
  const open = (): void => navigate({ name: 'instance', id: inst.id });
  return (
    <div
      class="cp-cover"
      role="button"
      tabIndex={0}
      aria-label={install.installing ? `Open ${inst.name}. ${installLabel}` : `Open ${inst.name}`}
      data-installing={install.installing}
      data-selected={selected === true}
      onClick={open}
      onKeyDown={(e) => openOnKey(e, open)}
      onContextMenu={onContextMenu}
    >
      <div class="cp-cover-art">
        <InstanceTile inst={inst} />
        {onToggleSelect && (
          <SelectionCheckbox
            selected={selected === true}
            label={selectionToggleLabel(selected === true, inst.name)}
            onToggle={(e) => {
              e.stopPropagation();
              onToggleSelect(e);
            }}
          />
        )}
        {!install.installing && !session && (
          <span class="cp-cover-play" aria-hidden="true">
            <Icon name={action === 'launch' ? 'play' : action === 'install' ? 'download' : 'alert'} size={14} />
          </span>
        )}
      </div>
      <strong class="cp-cover-name" title={inst.name}>
        {inst.name}
      </strong>
      <div class="cp-chips">
        {session ? (
          <SessionChip session={session} />
        ) : install.installing ? (
          <Chip tone="accent" icon={install.state === 'queued' ? 'clock' : 'download'}>
            {installLabel}
          </Chip>
        ) : (
          <InstanceFacts inst={inst} />
        )}
      </div>
    </div>
  );
}
