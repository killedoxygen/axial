import { subscribeApiEvents } from '../backend/events';
import { installQueueStateResponse } from '../dto-install';
import type { InstallQueueStateResponse } from '../types-install';

export function connectInstallQueueSSE(
  onSnapshot: (snapshot: InstallQueueStateResponse) => void,
  onError: (error: unknown) => void,
): () => void {
  return subscribeApiEvents('/install/queue/events', {
    decode: installQueueStateResponse,
    onValue(snapshot, _event, revision) {
      if (snapshot.revision !== revision) throw new Error('Install queue snapshot revision does not match its event.');
      onSnapshot(snapshot);
    },
    onError,
  });
}
