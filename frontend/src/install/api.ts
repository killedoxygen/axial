import { subscribeApiEvents } from '../platform/events';
import { request } from '../platform/transport';
import type { InstallQueueRequest } from '../generated/InstallQueueRequest';
import type { InstallQueueStateResponse } from '../generated/InstallQueueStateResponse';

export function queueSnapshot(): Promise<InstallQueueStateResponse> {
  return request('GET', '/install/queue');
}

export function enqueueInstall(
  body: InstallQueueRequest,
  options: { retry?: boolean; expectedInstallId?: string } = {},
): Promise<InstallQueueStateResponse> {
  const query = options.expectedInstallId
    ? `?expected_install_id=${encodeURIComponent(options.expectedInstallId)}`
    : '';
  return request('POST', `${options.retry ? '/install/queue/retry' : '/install/queue'}${query}`, body);
}

export function removeQueued(queueId: string): Promise<InstallQueueStateResponse> {
  return request('DELETE', `/install/queue/${encodeURIComponent(queueId)}`);
}

/** Dismisses a retained failure. */
export function dismissFailure(operationId: string): Promise<InstallQueueStateResponse> {
  return request('DELETE', `/install/queue/failures/${encodeURIComponent(operationId)}`);
}

export function subscribeQueue(
  onSnapshot: (snapshot: InstallQueueStateResponse) => void,
  onError: (error: unknown) => void,
): () => void {
  return subscribeApiEvents<InstallQueueStateResponse>('/install/queue/events', {
    onValue(snapshot, _event, revision) {
      if (snapshot.revision !== revision) throw new Error('Install queue snapshot revision does not match its event.');
      onSnapshot(snapshot);
    },
    onError,
  });
}
