import { apiResourceUrl, request } from '../platform/transport';
import type { MusicStatusResponse } from '../generated/MusicStatusResponse';

export function musicStatus(): Promise<MusicStatusResponse> {
  return request('GET', '/music/status');
}

export function trackUrl(track: number): string {
  return apiResourceUrl(`/music/track?t=${track}`);
}
