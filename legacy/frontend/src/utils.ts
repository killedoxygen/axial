import { collapsedLogSeverity, logLines } from './store';
import { toast } from './toast';

import type { LogSeverity } from './store';

export function cn(...inputs: unknown[]): string {
  return inputs.filter((v): v is string => typeof v === 'string' && v.length > 0).join(' ');
}

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

export function showError(msg: string): void {
  appendLog('stderr', `ERROR: ${msg}`);
  toast(msg, 'error');
}

export function errMessage(err: unknown): string {
  if (err instanceof Error && err.message) return err.message;
  if (typeof err === 'string') return err;
  return 'Unknown error';
}

/** Disk name of a mod without its `.disabled` suffix; the manifest keys mods by
 * this enabled-state base name. */
export function modBaseName(name: string): string {
  return name.toLowerCase().endsWith('.disabled') ? name.slice(0, -'.disabled'.length) : name;
}

export const USERNAME_MIN_LEN = 3;
export const USERNAME_MAX_LEN = 16;
export const USERNAME_PATTERN: RegExp = /^[A-Za-z0-9_]+$/;

export function validateUsername(raw: string): string | null {
  const v = raw.trim();
  if (v.length === 0) return 'Enter a name.';
  if (v.length < USERNAME_MIN_LEN) return `At least ${USERNAME_MIN_LEN} characters.`;
  if (v.length > USERNAME_MAX_LEN) return `At most ${USERNAME_MAX_LEN} characters.`;
  if (!USERNAME_PATTERN.test(v)) return 'Letters, numbers, and underscores only.';
  return null;
}

export function getMemoryRecommendation(totalGB: number): { rec: number; text: string } {
  if (totalGB <= 4) return { rec: 2, text: 'Low RAM: 2 GB recommended' };
  if (totalGB <= 8) return { rec: 4, text: '4 GB recommended' };
  if (totalGB <= 16) return { rec: 6, text: '6 GB recommended' };
  return { rec: 8, text: '8 GB recommended' };
}
