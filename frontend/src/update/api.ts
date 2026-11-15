import { request } from '../platform/transport';
import type { UpdateFlow } from '../generated/UpdateFlow';
import type { UpdateInfo } from '../generated/UpdateInfo';
import type { UpdateSnapshot } from '../generated/UpdateSnapshot';

export function checkForUpdate(force: boolean): Promise<UpdateInfo> {
  return request('GET', force ? '/update?force=1' : '/update');
}

export function updateFlow(): Promise<UpdateFlow> {
  return request('GET', '/update/flow');
}

export function updateSnapshot(): Promise<UpdateSnapshot> {
  return request('GET', '/update/snapshot');
}

export function downloadUpdate(version: string): Promise<UpdateFlow> {
  return request('POST', '/update/download', { version });
}

export function applyUpdate(): Promise<UpdateFlow> {
  return request('POST', '/update/apply');
}
