import { signal } from '@preact/signals';

export type LogSeverity = 'error' | 'system' | 'info';
export const logLines = signal(0);
export const collapsedLogSeverity = signal<LogSeverity | null>(null);
const SEVERITY_RANK: Record<LogSeverity, number> = { error: 3, system: 2, info: 1 };

function logSeverityFromSource(source: string): LogSeverity {
  if (source === 'stderr') return 'error';
  if (source === 'system') return 'system';
  return 'info';
}

function updateLogIndicator(source: string): void {
  const newSeverity = logSeverityFromSource(source);
  const currentSeverity = collapsedLogSeverity.value;
  if (currentSeverity && SEVERITY_RANK[currentSeverity] >= SEVERITY_RANK[newSeverity]) return;
  collapsedLogSeverity.value = newSeverity;
}

export function appendLog(source: string, text: string, instanceId?: string, instanceName?: string): void {
  void text;
  void instanceId;
  void instanceName;
  logLines.value += 1;
  updateLogIndicator(source);
}
