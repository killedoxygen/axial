import type { JSX } from 'preact';
import { useEffect, useMemo, useState } from 'preact/hooks';
import { Button, Pill } from '../../../ui/Atoms';
import { SelectField } from '../../../ui/Select';
import { Icon } from '../../../ui/Icons';
import { errMessage } from '../../../utils';
import type { EnrichedInstance, InstanceLogTail } from '../../../types';
import { fmtBytes, fmtRelative } from '../format';
import type { ResourceLoadState } from '../resources';
import {
  LOG_FILTER_LABELS,
  LOG_TAIL_POLL_MS,
  fetchLogTail,
  isCompressedLogArchive,
  isCurrentLog,
  pickInitialLog,
  sortLogs,
} from '../logs';
import type { LogFilter } from '../logs';
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
  const [filter, setFilter] = useState<LogFilter>('all');
  const [tail, setTail] = useState<{
    status: 'idle' | 'loading' | 'ready' | 'error';
    data?: InstanceLogTail;
    error?: string;
  }>({ status: 'idle' });
  const sortedLogs = useMemo(() => sortLogs(logs), [logs]);
  const selectedEntry = sortedLogs.find((log) => log.name === selected);
  const isLive = running && isCurrentLog(selected);
  const selectedIsCompressedArchive = isCompressedLogArchive(selected);

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
    if (!selected || selectedIsCompressedArchive) {
      setTail({ status: 'idle' });
      return;
    }
    let alive = true;
    const load = (showLoading: boolean): void => {
      if (showLoading) {
        setTail((current) => (current.data?.name === selected ? current : { status: 'loading' }));
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
  }, [inst.id, running, selected, selectedIsCompressedArchive]);

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
          <Button variant="secondary" size="sm" icon="refresh" onClick={onRefresh}>
            Refresh
          </Button>
          <Button variant="soft" size="sm" icon="folder" onClick={() => void openInstanceFolder(inst.id, 'logs')}>
            Open folder
          </Button>
        </div>
      </div>
      <ResourceStatus state={resources} onRetry={onRefresh} />
      {logs.length === 0 && resources.status !== 'loading' ? (
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
                options={sortedLogs.map((log) => ({
                  value: log.name,
                  label: isCurrentLog(log.name) ? `${log.name} (latest)` : log.name,
                }))}
              />
              {isLive && (
                <Pill tone="accent" icon="play">
                  Live
                </Pill>
              )}
            </div>
            {selectedEntry && (
              <span class="cp-logview-meta">
                {fmtBytes(selectedEntry.size)} · {fmtRelative(selectedEntry.modified_at)}
              </span>
            )}
          </div>
          <div class="cp-logview-body">
            {selectedIsCompressedArchive && (
              <div class="cp-logview-note">
                This is a compressed log archive. Croopor cannot preview .log.gz files here; use Open folder to extract
                it, or select an uncompressed .log file.
              </div>
            )}
            {!selectedIsCompressedArchive && tail.status === 'loading' && (
              <div class="cp-logview-note">Loading log…</div>
            )}
            {!selectedIsCompressedArchive && tail.status === 'error' && (
              <div class="cp-logview-note cp-logview-note--error">{tail.error}</div>
            )}
            {!selectedIsCompressedArchive && tail.status === 'ready' && (
              <>
                {tail.data?.truncated && (
                  <div class="cp-logview-truncated">
                    Showing the last {fmtBytes(tail.data.size > 0 ? Math.min(tail.data.size, 128 * 1024) : 0)} of this
                    log.
                  </div>
                )}
                <LogLines text={tail.data?.text ?? ''} filter={filter} />
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
