import type { JSX } from 'preact';
import { Icon } from '../../../ui/Icons';
import { Card } from '../../../ui/Atoms';
import type { EnrichedInstance } from '../../../types';
import { openInstanceFolder } from '../instance-actions';

export function QuickActionsCard({
  inst,
  running,
  onLaunch,
  onStop,
  onOpenLogs,
}: {
  inst: EnrichedInstance;
  running: boolean;
  onLaunch: () => void;
  onStop: () => void;
  onOpenLogs: () => void;
}): JSX.Element {
  return (
    <Card padding={18} class="cp-od-quick-card">
      <div class="cp-od-head">
        <h3>Quick actions</h3>
      </div>
      <div class="cp-od-quick-grid">
        <button
          class="cp-od-quick-action"
          type="button"
          onClick={() => void openInstanceFolder(inst.id, 'resourcepacks')}
        >
          <span class="cp-od-quick-icon">
            <Icon name="image" size={15} stroke={1.9} />
          </span>
          <span class="cp-od-quick-copy">
            <strong>Resource packs</strong>
            <span>Open resource packs</span>
          </span>
        </button>
        <button
          class="cp-od-quick-action"
          type="button"
          disabled={!running}
          onClick={() => {
            onStop();
            window.setTimeout(onLaunch, 450);
          }}
        >
          <span class="cp-od-quick-icon">
            <Icon name="refresh" size={15} stroke={1.9} />
          </span>
          <span class="cp-od-quick-copy">
            <strong>Restart</strong>
            <span>{running ? 'Restart the instance' : 'Available while running'}</span>
          </span>
        </button>
        <button class="cp-od-quick-action" type="button" data-tone="danger" disabled={!running} onClick={onStop}>
          <span class="cp-od-quick-icon">
            <Icon name="stop" size={15} stroke={1.9} />
          </span>
          <span class="cp-od-quick-copy">
            <strong>Stop</strong>
            <span>{running ? 'Stop the instance' : 'Not running'}</span>
          </span>
        </button>
        <button class="cp-od-quick-action" type="button" onClick={onOpenLogs}>
          <span class="cp-od-quick-icon">
            <Icon name="terminal" size={15} stroke={1.9} />
          </span>
          <span class="cp-od-quick-copy">
            <strong>Open logs</strong>
            <span>Inspect launch output</span>
          </span>
        </button>
      </div>
    </Card>
  );
}
