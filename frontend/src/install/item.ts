import type { InstallItem } from '../generated/InstallItem';
import type { InstallQueueRequest } from '../generated/InstallQueueRequest';

export function isSameInstallItem(left: InstallItem, right: InstallItem): boolean {
  if (left.version_id !== right.version_id) return false;
  if (left.content || right.content) {
    if (!left.content || !right.content) return false;
    return JSON.stringify(left.content) === JSON.stringify(right.content);
  }
  if (!left.loader && !right.loader) return true;
  if (!left.loader || !right.loader) return false;
  return left.loader.component_id === right.loader.component_id && left.loader.build_id === right.loader.build_id;
}

/** The queue request that installs this item again. */
export function installQueueRequest(item: InstallItem): InstallQueueRequest {
  if (item.content) {
    return {
      kind: 'content',
      instance_id: item.content.instance_id,
      label: item.content.label,
      action: item.content.action,
    };
  }
  if (!item.loader) return { kind: 'vanilla', version_id: item.version_id };
  return { kind: 'loader', component_id: item.loader.component_id, build_id: item.loader.build_id };
}
