import { apiUrl } from '../api';

export function connectLoaderInstallSSE(
  installId: string,
  onProgress: (data: any) => void,
  onError: (message: string) => void,
): EventSource {
  const es = new EventSource(apiUrl(`/loaders/install/${installId}/events`));

  es.addEventListener('progress', (e: MessageEvent) => {
    let data: any;
    try {
      data = JSON.parse(e.data);
    } catch {
      onError('Loader install progress data was invalid.');
      es.close();
      return;
    }
    onProgress(data);
    if (data.done || data.view_model?.terminal) {
      es.close();
    }
  });

  es.onerror = (): void => {
    if (es.readyState !== EventSource.CLOSED) return;
    onError('Loader install progress stopped unexpectedly.');
  };

  return es;
}
