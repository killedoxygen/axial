import type { JSX } from 'preact';
import { Icon } from '../../../ui/Icons';
import { Card } from '../../../ui/Atoms';
import type { InstanceResourceSummary } from '../../../types';
import { fmtRelative } from '../format';
import { pickInitialLog } from '../logs';

export function LogsCard({
  resources,
  onOpenLogs,
}: {
  resources: InstanceResourceSummary | null;
  onOpenLogs: () => void;
}): JSX.Element {
  const latest = pickInitialLog(resources?.logs ?? []);
  const latestLog = latest ? resources?.logs.find((log) => log.name === latest) : undefined;
  const count = resources?.logs_count ?? 0;
  const summary = latestLog ? `${latestLog.name} · ${fmtRelative(latestLog.modified_at)}` : 'No launch logs on disk yet';

  return (
    <Card padding={16} class="cp-od-logs-card">
      <div class="cp-od-logs-summary">
        <span class="cp-od-logs-icon"><Icon name="terminal" size={14} stroke={1.9} /></span>
        <div class="cp-od-logs-line">
          <strong>Logs</strong>
          <span class="cp-od-logs-sub">{summary}</span>
        </div>
        <button class="cp-od-link" type="button" onClick={onOpenLogs}>
          {count > 0 ? `View ${count}` : 'View logs'} <Icon name="chevron-right" size={11} stroke={2.2} />
        </button>
      </div>
    </Card>
  );
}
