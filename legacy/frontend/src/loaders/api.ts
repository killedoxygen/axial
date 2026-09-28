import { apiEventSourceUrl } from '../api';
import { isDtoRecord } from '../dto-contract';

export async function connectLoaderInstallSSE(
  installId: string,
  onProgress: (data: unknown) => void,
  onError: (message: string) => void,
): Promise<EventSource> {
  const es = new EventSource(await apiEventSourceUrl(`/loaders/install/${installId}/events`));

  es.addEventListener('progress', (e: MessageEvent) => {
    let data: unknown;
    try {
      data = JSON.parse(e.data);
    } catch {
      onError('Loader install progress data was invalid.');
      es.close();
      return;
    }
    onProgress(data);
    const record = isDtoRecord(data) ? data : null;
    const view = isDtoRecord(record?.view_model) ? record.view_model : null;
    if (record?.done === true || view?.terminal === true) {
      es.close();
    }
  });

  es.onerror = (): void => {
    if (es.readyState !== EventSource.CLOSED) return;
    onError('Loader install progress stopped unexpectedly.');
  };

  return es;
}
