import type { SessionNotice } from '../generated/SessionNotice';

const MAX_TRACKED_NOTICES = 16;

/** Shows each backend notice of one launch once, however many snapshots repeat it. */
export interface LaunchNoticeTracker {
  consume(notice: SessionNotice | null): SessionNotice | null;
}

export function createLaunchNoticeTracker(): LaunchNoticeTracker {
  const seen = new Set<string>();
  const order: string[] = [];
  return {
    consume(notice) {
      if (!notice?.message.trim()) return null;
      const key = JSON.stringify([notice.tone, notice.message.trim()]);
      if (seen.has(key)) return null;
      if (order.length === MAX_TRACKED_NOTICES) seen.delete(order.shift() as string);
      order.push(key);
      seen.add(key);
      return notice;
    },
  };
}
