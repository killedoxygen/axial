import type { JSX } from 'preact';
import { useMemo } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Card } from '../../../ui/Atoms';
import type { EnrichedInstance, InstanceResourceSummary } from '../../../types';
import { fmtRelative } from '../format';

interface ActivityItem {
  label: string;
  relative: string;
}

export function ActivityCard({
  inst,
  resources,
  onOpenLogs,
}: {
  inst: EnrichedInstance;
  resources: InstanceResourceSummary | null;
  onOpenLogs: () => void;
}): JSX.Element {
  const events: ActivityItem[] = useMemo(() => {
    const out: ActivityItem[] = [];
    out.push({ label: 'Instance created', relative: fmtRelative(inst.created_at) });
    if (inst.last_played_at) {
      out.unshift({ label: 'Last launch session', relative: fmtRelative(inst.last_played_at) });
    }
    const latestLog = resources?.logs[0];
    if (latestLog) {
      out.push({ label: `Latest log: ${latestLog.name}`, relative: fmtRelative(latestLog.modified_at) });
    }
    const latestWorld = resources?.worlds[0];
    if (latestWorld) {
      out.push({ label: `World changed: ${latestWorld.name}`, relative: fmtRelative(latestWorld.modified_at) });
    }
    return out.slice(0, 3);
  }, [inst.id, inst.created_at, inst.last_played_at, resources]);

  return (
    <Card padding={18}>
      <div class="cp-od-head cp-od-head--iconed">
        <div class="cp-od-head-tile">
          <Icon name="activity" size={13} stroke={1.9} />
        </div>
        <h3>Activity</h3>
        <button class="cp-od-link" type="button" onClick={onOpenLogs}>
          View all <Icon name="chevron-right" size={11} stroke={2.2} />
        </button>
      </div>
      <ul class="cp-od-events">
        {events.map((e, i) => (
          <li key={i} class="cp-od-event">
            <span class="cp-od-event-dot" aria-hidden="true" />
            <span class="cp-od-event-msg">{e.label}</span>
            <span class="cp-od-event-rel">{e.relative}</span>
          </li>
        ))}
      </ul>
    </Card>
  );
}
