import { subscribeApiEvents } from '../platform/events';
import { isApiError, request } from '../platform/transport';
import type { LaunchErrorResponse } from '../generated/LaunchErrorResponse';
import type { LaunchIntentView } from '../generated/LaunchIntentView';
import type { LaunchRequest } from '../generated/LaunchRequest';
import type { LogEntry } from '../generated/LogEntry';
import type { SessionLogsResponse } from '../generated/SessionLogsResponse';
import type { SessionSnapshot } from '../generated/SessionSnapshot';
import type { SessionsResponse } from '../generated/SessionsResponse';

const session = (id: string): string => `/launch/${encodeURIComponent(id)}`;

export function launch(body: LaunchRequest): Promise<SessionSnapshot> {
  return request('POST', '/launch', body);
}

export function launchIntent(intentKey: string): Promise<LaunchIntentView> {
  return request('GET', `/launch/intents/${encodeURIComponent(intentKey)}`);
}

export function listSessions(): Promise<SessionsResponse> {
  return request('GET', '/launch/sessions');
}

export function sessionStatus(sessionId: string): Promise<SessionSnapshot> {
  return request('GET', `${session(sessionId)}/status`);
}

export function stopSession(sessionId: string): Promise<SessionSnapshot> {
  return request('POST', `${session(sessionId)}/kill`);
}

export async function sessionLogs(sessionId: string): Promise<LogEntry[]> {
  return (await request<SessionLogsResponse>('GET', `${session(sessionId)}/logs`)).entries;
}

/** Live status snapshots and log lines; a reconnect starts with a fresh snapshot. */
export function subscribeSession(
  sessionId: string,
  handlers: {
    status(snapshot: SessionSnapshot): void;
    log(entry: LogEntry): void;
    error(): void;
  },
): () => void {
  return subscribeApiEvents<SessionSnapshot | LogEntry>(`${session(sessionId)}/events`, {
    events: ['status', 'log'],
    allowBareEvents: true,
    onValue: (value, eventName) => {
      if (eventName === 'status') handlers.status(value as SessionSnapshot);
      else handlers.log(value as LogEntry);
    },
    onError: handlers.error,
  });
}

/** The launch owner's refusal, when the error is one. */
export function launchRefusal(error: unknown): LaunchErrorResponse | null {
  if (!isApiError(error) || typeof error.payload !== 'object' || error.payload === null) return null;
  return 'code' in error.payload ? (error.payload as LaunchErrorResponse) : null;
}
