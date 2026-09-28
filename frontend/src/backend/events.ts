import { apiEventSourceUrl } from '../api';

export interface ApiEventOptions<T> {
  decode(value: unknown): T;
  onValue(value: T, eventName: string, revision: number | null): void;
  onError?(error: unknown): void;
  events?: readonly string[];
  /** Compatibility for retained named domain events during producer migration. */
  allowLegacyEvents?: boolean;
}

/** A reconnect is a fresh authenticated read, never a replay of a domain command. */
export function subscribeApiEvents<T>(path: string, options: ApiEventOptions<T>): () => void {
  let closed = false;
  let current: EventSource | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let failures = 0;
  let notified = false;
  const eventNames = [...new Set(options.events ?? ['message'])];

  function reconnect(error: unknown): void {
    if (closed) return;
    current?.close();
    current = null;
    if (timer !== undefined) return;
    if (!notified) {
      notified = true;
      try { options.onError?.(error); } catch { /* An observer cannot stop recovery. */ }
    }
    const delay = Math.min(10_000, 250 * 2 ** Math.min(failures++, 6));
    timer = setTimeout(() => { timer = undefined; void connect(); }, delay);
  }

  async function connect(): Promise<void> {
    try {
      const url = await apiEventSourceUrl(path);
      if (closed) return;
      const source = new EventSource(url);
      // The first snapshot of a new connection must reach its domain owner,
      // including after a backend restart or a failed dependent read.
      const revisions = new Map<string, number>();
      current = source;
      source.onopen = () => {
        if (source !== current || closed) return;
        failures = 0;
        notified = false;
      };
      for (const name of eventNames) {
        source.addEventListener(name, (event) => {
          if (source !== current || closed) return;
          try {
            const decoded: unknown = JSON.parse((event as MessageEvent<string>).data);
            const record = decoded !== null && typeof decoded === 'object' && !Array.isArray(decoded)
              ? decoded as Record<string, unknown> : null;
            const enveloped = record !== null
              && Object.prototype.hasOwnProperty.call(record, 'revision')
              && Object.prototype.hasOwnProperty.call(record, 'value');
            let revision: number | null = null;
            if (enveloped) {
              if (!Number.isSafeInteger(record.revision) || (record.revision as number) < 0) {
                throw new Error('API event revision is invalid.');
              }
              revision = record.revision as number;
              if (revision <= (revisions.get(name) ?? -1)) return;
            } else if (!options.allowLegacyEvents) {
              throw new Error('API event snapshot is invalid.');
            }
            const value = options.decode(enveloped ? record.value : decoded);
            options.onValue(value, name, revision);
            if (revision !== null) revisions.set(name, revision);
            failures = 0;
            notified = false;
          } catch (error) { reconnect(error); }
        });
      }
      source.onerror = () => {
        if (source === current) reconnect(new Error('API event connection was interrupted.'));
      };
    } catch (error) { reconnect(error); }
  }

  void connect();
  return () => {
    closed = true;
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
    current?.close();
    current = null;
  };
}
