import type { LaunchNotice } from './types-launch';

const MAX_TRACKED_NOTICES = 16;

function primaryNoticeDetail(details: string[]): string {
  return details[0] || '';
}

export function backendLaunchNotice(value: unknown): LaunchNotice | null {
  if (!value || typeof value !== 'object') return null;
  const candidate = value as Partial<LaunchNotice>;
  if (typeof candidate.message !== 'string' || !candidate.message.trim()) return null;
  if (
    candidate.tone !== 'info' &&
    candidate.tone !== 'success' &&
    candidate.tone !== 'warned' &&
    candidate.tone !== 'intervened' &&
    candidate.tone !== 'error'
  ) {
    return null;
  }
  const details = Array.isArray(candidate.details)
    ? candidate.details.filter((detail): detail is string => typeof detail === 'string' && Boolean(detail.trim()))
    : [];
  const detail =
    typeof candidate.detail === 'string' && candidate.detail.trim() ? candidate.detail : primaryNoticeDetail(details);
  return {
    message: candidate.message,
    detail,
    details,
    tone: candidate.tone,
  };
}

function normalizedNoticeKey(notice: LaunchNotice): string {
  return JSON.stringify([
    notice.tone,
    notice.message.trim(),
    notice.detail?.trim() || '',
    (notice.details || []).map((detail) => detail.trim()).filter(Boolean),
  ]);
}

export interface BackendLaunchNoticeTracker {
  consume(value: unknown): LaunchNotice | null;
}

export function createBackendLaunchNoticeTracker(): BackendLaunchNoticeTracker {
  const seen = new Set<string>();
  const order: string[] = [];

  return {
    consume(value: unknown): LaunchNotice | null {
      const notice = backendLaunchNotice(value);
      if (!notice) return null;

      const key = normalizedNoticeKey(notice);
      if (seen.has(key)) return null;
      if (order.length === MAX_TRACKED_NOTICES) {
        const oldest = order.shift();
        if (oldest) seen.delete(oldest);
      }
      order.push(key);
      seen.add(key);
      return notice;
    },
  };
}
