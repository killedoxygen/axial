import type { ToastKind } from './types-ui';

export interface CreateNotice {
  state_id: string;
  tone: string;
  message: string;
  detail?: string | null;
}

export interface CreateResultPresentationSource {
  view_model?: {
    state_id?: string;
    tone?: string;
    title?: string;
    summary?: string;
    detail?: string | null;
  };
  guardian_notice?: {
    state_id?: string;
    tone?: string;
    message?: string;
    detail?: string | null;
  };
}

function trimmed(value: unknown): string {
  return typeof value === 'string' ? value.trim() : '';
}

export function createToastKind(tone: string | undefined): ToastKind {
  if (tone === 'error') return 'error';
  if (tone === 'warn') return 'info';
  return 'success';
}

function appendUnique(parts: string[], value: string): void {
  if (!value || parts.some((part) => part.includes(value))) return;
  parts.push(value);
}

export function createResultToastMessage(source: CreateResultPresentationSource): string {
  const summary = trimmed(source.view_model?.summary);
  const detail = trimmed(source.view_model?.detail);
  const guardianMessage = trimmed(source.guardian_notice?.message);
  const guardianDetail = trimmed(source.guardian_notice?.detail);
  const parts: string[] = [];

  appendUnique(parts, summary);
  appendUnique(parts, guardianMessage);
  appendUnique(parts, detail);
  appendUnique(parts, guardianDetail);
  return parts.join(' ');
}

function noticeTone(value: string): string {
  if (value === 'warn' || value === 'warned') return 'warned';
  if (value === 'error') return 'error';
  if (value === 'intervened') return 'intervened';
  if (value === 'success') return 'success';
  return 'info';
}

function noticeIcon(tone: string): string {
  if (tone === 'success') return 'check-circle';
  if (tone === 'error' || tone === 'warned') return 'alert';
  if (tone === 'intervened') return 'shield-check';
  return 'info';
}

export function createNoticePresentation(notice: CreateNotice): { tone: string; icon: string } {
  const tone = noticeTone(notice.tone);
  return { tone, icon: noticeIcon(tone) };
}
