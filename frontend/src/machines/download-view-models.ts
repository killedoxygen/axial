import type { InstallQueueNoticeViewModel } from '../types-install';

export function queueNoticeToastKind(notice: InstallQueueNoticeViewModel): 'success' | 'error' | 'info' {
  if (notice.tone === 'error' || notice.tone === 'err') return 'error';
  if (notice.tone === 'warn' || notice.tone === 'warning') return 'info';
  return notice.state_id === 'queued' || notice.state_id === 'retry_queued' ? 'success' : 'info';
}

export function installQueueNoticePresentation(
  notice: InstallQueueNoticeViewModel | null | undefined,
): { message: string; kind: 'success' | 'error' | 'info' } | null {
  if (!notice?.message?.trim()) return null;
  const message = notice.detail?.trim() ? `${notice.message.trim()}: ${notice.detail.trim()}` : notice.message.trim();
  return { message, kind: queueNoticeToastKind(notice) };
}
