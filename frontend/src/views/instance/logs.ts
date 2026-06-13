import { api } from '../../api';
import type { InstanceResourceSummary, InstanceLogTail } from '../../types';

export type InstanceLogEntry = InstanceResourceSummary['logs'][number];
export type LogFilter = 'all' | 'important' | 'errors' | 'warnings' | 'system-info';
export type LogLineKind = 'error' | 'warning' | 'system' | 'info';

export interface ClassifiedLogLine {
  index: number;
  text: string;
  kind: LogLineKind;
  label: string;
  important: boolean;
}

export const LOG_FILTER_LABELS: Record<LogFilter, string> = {
  all: 'All',
  important: 'Important',
  errors: 'Errors',
  warnings: 'Warnings',
  'system-info': 'System/info',
};

export const LOG_TAIL_POLL_MS = 2500;
export const LOG_RESOURCE_POLL_MS = 10000;

export function currentLogRank(name: string): number {
  const lower = name.toLowerCase();
  if (lower === 'latest.log') return 0;
  if (lower === 'current.log') return 1;
  if (lower.includes('latest')) return 2;
  if (lower.includes('current')) return 3;
  return 10;
}

export function isCurrentLog(name: string): boolean {
  return currentLogRank(name) < 10;
}

export function isCompressedLogArchive(name: string): boolean {
  return name.toLowerCase().endsWith('.log.gz');
}

export function sortLogs(logs: InstanceLogEntry[]): InstanceLogEntry[] {
  const next = [...logs];
  next.sort((a, b) => {
    const current = currentLogRank(a.name) - currentLogRank(b.name);
    if (current !== 0) return current;
    return b.modified_at.localeCompare(a.modified_at) || a.name.localeCompare(b.name);
  });
  return next;
}

export function pickInitialLog(logs: InstanceLogEntry[]): string {
  return sortLogs(logs)[0]?.name ?? '';
}

export function classifyLogLine(text: string): LogLineKind {
  const lower = text.toLowerCase();
  if (/\b(errors?|fatal|exceptions?|crashes?|crashed)\b/.test(lower)) return 'error';
  if (/\bwarn(?:ing|ings|ed)?\b/.test(lower)) return 'warning';
  if (/\b(launcher|system|guardian|healing|croopor)\b/.test(lower)) return 'system';
  return 'info';
}

export function logLineLabel(kind: LogLineKind): string {
  if (kind === 'error') return 'ERR';
  if (kind === 'warning') return 'WARN';
  if (kind === 'system') return 'SYS';
  return 'INFO';
}

export function classifyLogText(text: string): ClassifiedLogLine[] {
  if (!text) return [];
  const normalized = text.replace(/\r\n?/g, '\n');
  const rawLines = normalized.endsWith('\n') ? normalized.slice(0, -1).split('\n') : normalized.split('\n');
  return rawLines.map((line, index) => {
    const kind = classifyLogLine(line);
    return {
      index,
      text: line,
      kind,
      label: logLineLabel(kind),
      important: kind !== 'info',
    };
  });
}

export function logLineMatchesFilter(line: ClassifiedLogLine, filter: LogFilter): boolean {
  if (filter === 'all') return true;
  if (filter === 'important') return line.important;
  if (filter === 'errors') return line.kind === 'error';
  if (filter === 'warnings') return line.kind === 'warning';
  return line.kind === 'system' || line.kind === 'info';
}

export async function fetchLogTail(id: string, name: string): Promise<InstanceLogTail> {
  const res: InstanceLogTail & { error?: string } = await api('GET', `/instances/${encodeURIComponent(id)}/logs/${encodeURIComponent(name)}`);
  if (res?.error) throw new Error(res.error);
  return res;
}
