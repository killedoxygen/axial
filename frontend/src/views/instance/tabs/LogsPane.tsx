import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { Button, Pill } from '../../../ui/Atoms';
import { SelectField } from '../../../ui/Select';
import { Icon } from '../../../ui/Icons';
import { formatBytes, fmtRelative } from '../../../format';
import { errMessage } from '../../../utils';
import { launchSessions } from '../../../store';
import type { EnrichedInstance, InstanceLogTail } from '../../../types-instance';
import type { ResourceLoadState } from '../resources';
import {
  LOG_FILTER_LABELS,
  LOG_TAIL_POLL_MS,
  fetchLogTail,
  fetchSessionLog,
  isCompressedLogArchive,
  isCurrentLog,
  pickInitialLog,
  sortLogs,
} from '../logs';
import type { LogFilter } from '../logs';
import { openInstanceFolder } from '../instance-actions';
import { ResourceEmpty, ResourceStatus } from '../components/resource-bits';
import { LogLines } from '../components/log-line';

type LogsPaneProps = {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  processLive: boolean;
  onRefresh: () => void;
};

export function LogsPane(props: LogsPaneProps): JSX.Element {
  return <InstanceLogsPane key={props.inst.id} {...props} />;
}

function InstanceLogsPane({ inst, resources, processLive, onRefresh }: LogsPaneProps): JSX.Element {
  const logs = resources.data?.logs ?? [];
  const sessionId = launchSessions.value[inst.id]?.sessionId;
  const sessionLog = sessionId ? `session:${sessionId}` : '';
  const [selection, setSelected] = useState<string>(sessionLog);
  const [filter, setFilter] = useState<LogFilter>('all');
  const [refreshRevision, setRefreshRevision] = useState(0);
  const [tail, setTail] = useState<{
    name?: string;
    status: 'idle' | 'loading' | 'ready' | 'error';
    data?: Pick<InstanceLogTail, 'text' | 'truncated'> & { size?: number };
    error?: string;
  }>({ status: 'idle' });
  const sortedLogs = useMemo(() => sortLogs(logs), [logs]);
  const selected =
    (sessionLog && selection === sessionLog) || sortedLogs.some((log) => log.name === selection)
      ? selection
      : sessionLog || pickInitialLog(logs);
  const selectedEntry = sortedLogs.find((log) => log.name === selected);
  const sessionSelected = Boolean(sessionLog && selected === sessionLog);
  const isLive = processLive && (sessionSelected || isCurrentLog(selected));
  const selectedIsCompressedArchive = isCompressedLogArchive(selected);
  const currentTail = tail.name === selected ? tail : { status: 'loading' as const };
  const refresh = (): void => {
    setRefreshRevision((revision) => revision + 1);
    onRefresh();
  };

  useEffect(() => {
    if (sessionLog) setSelected(sessionLog);
  }, [sessionLog]);

  useEffect(() => {
    if (!selected || selectedIsCompressedArchive) {
      setTail({ status: 'idle' });
      return;
    }
    let alive = true;
    let inFlight = false;
    const isCurrent = (): boolean => alive && launchSessions.value[inst.id]?.sessionId === sessionId;
    const load = (showLoading: boolean): void => {
      if (inFlight) return;
      inFlight = true;
      if (showLoading) {
        setTail({ status: 'loading', name: selected });
      }
      const request = sessionSelected && sessionId ? fetchSessionLog(sessionId) : fetchLogTail(inst.id, selected);
      void request
        .then((data) => {
          if (isCurrent()) setTail({ status: 'ready', name: selected, data });
        })
        .catch((err) => {
          if (isCurrent()) setTail({ status: 'error', name: selected, error: errMessage(err) });
        })
        .finally(() => {
          inFlight = false;
        });
    };
    load(true);
    const timer = sessionSelected || isLive ? window.setInterval(() => load(false), LOG_TAIL_POLL_MS) : 0;
    return () => {
      alive = false;
      if (timer) window.clearInterval(timer);
    };
  }, [inst.id, isLive, selected, selectedIsCompressedArchive, refreshRevision, sessionId, sessionSelected]);

  return (
    <div class="cp-instance-body cp-logs-pane">
      <div class="cp-resource-toolbar cp-logs-toolbar">
        <strong>Logs</strong>
        <div class="cp-logs-tools">
          <div class="cp-mini-seg" role="tablist" aria-label="Filter log lines">
            {(Object.keys(LOG_FILTER_LABELS) as LogFilter[]).map((item) => (
              <button
                key={item}
                type="button"
                role="tab"
                aria-selected={filter === item}
                data-active={filter === item}
                onClick={() => setFilter(item)}
              >
                {LOG_FILTER_LABELS[item]}
              </button>
            ))}
          </div>
          <Button variant="secondary" size="sm" icon="refresh" onClick={refresh}>
            Refresh
          </Button>
          <Button variant="secondary" size="sm" icon="folder" onClick={() => void openInstanceFolder(inst.id, 'logs')}>
            Open folder
          </Button>
        </div>
      </div>
      {!sessionSelected && <ResourceStatus state={resources} onRetry={refresh} />}
      {!sessionLog && logs.length === 0 && resources.status === 'ready' ? (
        <ResourceEmpty
          icon="terminal"
          title="No logs yet"
          hint="Launch this instance and Minecraft log files will appear here."
        />
      ) : (
        <div class="cp-logview">
          <div class="cp-logview-bar">
            <div class="cp-logview-pick">
              <Icon name="terminal" size={14} color="var(--text-mute)" />
              <SelectField
                value={selected}
                onChange={setSelected}
                ariaLabel="Log file"
                width={260}
                options={[
                  ...(sessionLog ? [{ value: sessionLog, label: 'Session output' }] : []),
                  ...sortedLogs.map((log) => ({
                    value: log.name,
                    label: isCurrentLog(log.name) ? `${log.name} (latest)` : log.name,
                  })),
                ]}
              />
              {isLive && (
                <Pill tone="accent" icon="play">
                  Live
                </Pill>
              )}
            </div>
            {selectedEntry && (
              <span class="cp-logview-meta">
                {formatBytes(selectedEntry.size)}, {fmtRelative(selectedEntry.modified_at)}
              </span>
            )}
          </div>
          <div class="cp-logview-body">
            {selectedIsCompressedArchive && (
              <div class="cp-logview-note">
                This is a compressed log archive. Axial cannot preview .log.gz files here; use Open folder to extract
                it, or select an uncompressed .log file.
              </div>
            )}
            {!selectedIsCompressedArchive && currentTail.status === 'loading' && (
              <div class="cp-logview-note">Loading log…</div>
            )}
            {!selectedIsCompressedArchive && currentTail.status === 'error' && (
              <div class="cp-logview-note cp-logview-note--error">{currentTail.error}</div>
            )}
            {!selectedIsCompressedArchive && currentTail.status === 'ready' && (
              <>
                {currentTail.data?.truncated && (
                  <div class="cp-logview-truncated">
                    {sessionSelected ? (
                      'Some session output was truncated.'
                    ) : (
                      <>Showing the last {formatBytes(Math.min(currentTail.data.size ?? 0, 128 * 1024))} of this log.</>
                    )}
                  </div>
                )}
                {sessionSelected && !currentTail.data?.text ? (
                  <div class="cp-log-empty">No session output yet.</div>
                ) : (
                  <LogLines text={currentTail.data?.text ?? ''} filter={filter} />
                )}
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
