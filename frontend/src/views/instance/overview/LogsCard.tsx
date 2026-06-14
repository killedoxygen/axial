import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Card } from '../../../ui/Atoms';
import { errMessage } from '../../../utils';
import type { InstanceLogTail, InstanceResourceSummary } from '../../../types';
import { fmtRelative } from '../format';
import { classifyLogText, fetchLogTail, LOG_TAIL_POLL_MS, pickInitialLog } from '../logs';
import { LogLine } from '../components/log-line';

type LogsCardTailState =
  | { status: 'idle' }
  | { status: 'loading'; name: string }
  | { status: 'ready'; data: InstanceLogTail }
  | { status: 'error'; name: string; error: string };

export function LogsCard({
  instanceId,
  resources,
  running,
  onOpenLogs,
}: {
  instanceId: string;
  resources: InstanceResourceSummary | null;
  running: boolean;
  onOpenLogs: () => void;
}): JSX.Element {
  const latest = pickInitialLog(resources?.logs ?? []);
  const latestLog = latest ? resources?.logs.find((log) => log.name === latest) : undefined;
  const count = resources?.logs_count ?? 0;
  const summary = latestLog
    ? `${latestLog.name} · ${fmtRelative(latestLog.modified_at)}`
    : 'No launch logs on disk yet';
  const [tail, setTail] = useState<LogsCardTailState>({ status: 'idle' });
  const importantLines = useMemo(() => {
    if (tail.status !== 'ready' || tail.data.name !== latest || !tail.data.text) return [];
    return classifyLogText(tail.data.text)
      .filter((line) => line.important && line.text.trim())
      .slice(-2);
  }, [latest, tail]);
  const previewNote = (() => {
    if (!latest) return '';
    if (tail.status === 'error' && tail.name === latest) return tail.error || 'Could not read latest log.';
    if (tail.status === 'ready' && tail.data.name === latest) return 'No warnings or errors in the latest log tail.';
    return 'Reading latest log...';
  })();

  useEffect(() => {
    if (!latest) {
      setTail({ status: 'idle' });
      return;
    }
    let alive = true;
    const load = (showLoading: boolean): void => {
      if (showLoading) {
        setTail((current) =>
          current.status === 'ready' && current.data.name === latest ? current : { status: 'loading', name: latest },
        );
      }
      void fetchLogTail(instanceId, latest)
        .then((data) => {
          if (alive) setTail({ status: 'ready', data });
        })
        .catch((err) => {
          if (alive) setTail({ status: 'error', name: latest, error: errMessage(err) });
        });
    };
    load(true);
    const timer = running ? window.setInterval(() => load(false), LOG_TAIL_POLL_MS) : 0;
    return () => {
      alive = false;
      if (timer) window.clearInterval(timer);
    };
  }, [instanceId, latest, running]);

  return (
    <Card padding={16} class="cp-od-logs-card">
      <div class="cp-od-logs-summary">
        <span class="cp-od-logs-icon">
          <Icon name="terminal" size={14} stroke={1.9} />
        </span>
        <div class="cp-od-logs-line">
          <strong>Logs</strong>
          <span class="cp-od-logs-sub">{summary}</span>
        </div>
        <button class="cp-od-link" type="button" onClick={onOpenLogs}>
          {count > 0 ? `View ${count}` : 'View logs'} <Icon name="chevron-right" size={11} stroke={2.2} />
        </button>
      </div>
      {latest && (
        <div class="cp-od-logs-preview" aria-label="Important log lines">
          {importantLines.length > 0 ? (
            importantLines.map((line) => <LogLine key={line.index} line={line} compact />)
          ) : (
            <div class="cp-od-logs-preview-note">{previewNote}</div>
          )}
        </div>
      )}
    </Card>
  );
}
