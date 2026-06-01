import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { Button } from '../../../ui/Atoms';
import { errMessage } from '../../../utils';
import type { EnrichedInstance, InstanceLogTail } from '../../../types';
import { fmtBytes, fmtRelative } from '../format';
import type { ResourceLoadState } from '../resources';
import {
  LOG_FILTER_LABELS,
  LOG_SORT_LABELS,
  LOG_TAIL_POLL_MS,
  fetchLogTail,
  isCurrentLog,
  pickInitialLog,
  sortLogs,
} from '../logs';
import type { LogFilter, LogSort } from '../logs';
import { openInstanceFolder } from '../instance-actions';
import { ResourceEmpty, ResourceStatus } from '../components/resource-bits';
import { LogLines } from '../components/log-line';

export function LogsPane({
  inst,
  resources,
  running,
  onRefresh,
}: {
  inst: EnrichedInstance;
  resources: ResourceLoadState;
  running: boolean;
  onRefresh: () => void;
}): JSX.Element {
  const logs = resources.data?.logs ?? [];
  const [selected, setSelected] = useState<string>('');
  const [sort, setSort] = useState<LogSort>('current');
  const [filter, setFilter] = useState<LogFilter>('all');
  const [tail, setTail] = useState<{ status: 'idle' | 'loading' | 'ready' | 'error'; data?: InstanceLogTail; error?: string }>({ status: 'idle' });
  const sortedLogs = useMemo(() => sortLogs(logs, sort), [logs, sort]);

  useEffect(() => {
    if (!logs.length) {
      setSelected('');
      return;
    }
    if (!selected || !logs.some((log) => log.name === selected)) {
      setSelected(pickInitialLog(logs));
    }
  }, [logs, selected]);

  useEffect(() => {
    if (!selected) {
      setTail({ status: 'idle' });
      return;
    }
    let alive = true;
    const load = (showLoading: boolean): void => {
      if (showLoading) {
        setTail((current) => current.data?.name === selected ? current : { status: 'loading' });
      }
      void fetchLogTail(inst.id, selected)
        .then((data) => {
          if (alive) setTail({ status: 'ready', data });
        })
        .catch((err) => {
          if (alive) setTail({ status: 'error', error: errMessage(err) });
        });
    };
    load(true);
    const timer = running ? window.setInterval(() => load(false), LOG_TAIL_POLL_MS) : 0;
    return () => {
      alive = false;
      if (timer) window.clearInterval(timer);
    };
  }, [inst.id, running, selected]);

  return (
    <div class="cp-instance-body cp-logs-pane">
      <div class="cp-resource-toolbar cp-logs-toolbar">
        <strong>{logs.length} log file{logs.length === 1 ? '' : 's'}</strong>
        <div class="cp-logs-tools">
          <div class="cp-mini-seg" role="tablist" aria-label="Sort logs">
            {(Object.keys(LOG_SORT_LABELS) as LogSort[]).map((item) => (
              <button
                key={item}
                type="button"
                role="tab"
                aria-selected={sort === item}
                data-active={sort === item}
                onClick={() => setSort(item)}
              >
                {LOG_SORT_LABELS[item]}
              </button>
            ))}
          </div>
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
          <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>Refresh</Button>
          <Button variant="soft" size="sm" icon="folder" onClick={() => void openInstanceFolder(inst.id, 'logs')}>Open logs</Button>
        </div>
      </div>
      <ResourceStatus state={resources} onRetry={onRefresh} />
      {logs.length === 0 && resources.status !== 'loading' ? (
        <ResourceEmpty icon="terminal" title="No logs yet" hint="Launch this instance and Minecraft log files will appear here." />
      ) : (
        <div class="cp-logs-layout">
          <div class="cp-logs-list">
            {sortedLogs.map((log) => (
              <button key={log.name} type="button" data-active={selected === log.name} onClick={() => setSelected(log.name)}>
                <span>{log.name}</span>
                <small>{isCurrentLog(log.name) ? 'Current/latest · ' : ''}{fmtBytes(log.size)} · {fmtRelative(log.modified_at)}</small>
              </button>
            ))}
          </div>
          <div class="cp-log-preview">
            {tail.status === 'loading' && <div class="cp-resource-note">Loading log preview…</div>}
            {tail.status === 'error' && <div class="cp-resource-note cp-resource-note--error">{tail.error}</div>}
            {tail.status === 'ready' && (
              <>
                {tail.data?.truncated && <div class="cp-log-truncated">Showing the last {fmtBytes(tail.data.size > 0 ? Math.min(tail.data.size, 128 * 1024) : 0)} of this log.</div>}
                <LogLines text={tail.data?.text ?? ''} filter={filter} />
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
