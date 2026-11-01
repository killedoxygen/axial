import { apiFetch, apiUrl } from './api';
import { isDtoRecord } from './dto-contract';

/** POST a file as the raw request body; the route owns its size and type bounds. */
export async function uploadFile(path: string, file: Blob): Promise<unknown> {
  if (__AXIAL_MOCK_API__) {
    const { mockApi } = await import('./mock/api');
    return mockApi('POST', path, file);
  }
  const response = await apiFetch(apiUrl(path), {
    method: 'POST',
    headers: { 'Content-Type': 'application/octet-stream' },
    body: file,
  });
  const payload: unknown = await response.json().catch(() => undefined);
  if (!response.ok) {
    const message = isDtoRecord(payload) && typeof payload.error === 'string' ? payload.error : '';
    throw Object.assign(new Error(message || `Upload failed with HTTP ${response.status}`), {
      name: 'ApiError',
      status: response.status,
      payload,
    });
  }
  return payload;
}
