import type {
  InstallItem,
  InstallQueueInstallItemViewModel,
  InstallQueueRequest,
  InstallQueuedItemViewModel,
} from './types-install';

export function cloneInstallItem(item: InstallItem): InstallItem {
  if (item.loader) return { versionId: item.versionId, loader: { ...item.loader } };
  if (item.content) return { versionId: item.versionId, content: structuredClone(item.content) };
  return { versionId: item.versionId };
}

export function isSameInstallItem(left: InstallItem, right: InstallItem): boolean {
  if (left.versionId !== right.versionId) return false;
  if (left.content || right.content) {
    if (!left.content || !right.content) return false;
    return JSON.stringify(left.content) === JSON.stringify(right.content);
  }
  if (!left.loader && !right.loader) return true;
  if (!left.loader || !right.loader) return false;
  return left.loader.componentId === right.loader.componentId && left.loader.buildId === right.loader.buildId;
}

export function installItemFromQueueInstallItem(value: InstallQueueInstallItemViewModel): InstallItem {
  const versionId = value.version_id;
  if (value.content) return { versionId, content: value.content };
  if (!value.loader) return { versionId };
  return {
    versionId,
    loader: {
      componentId: value.loader.component_id,
      buildId: value.loader.build_id,
      minecraftVersion: value.loader.minecraft_version,
      loaderVersion: value.loader.loader_version,
    },
  };
}

export function installItemFromQueuedViewModel(item: InstallQueuedItemViewModel): InstallItem {
  return installItemFromQueueInstallItem(item.install_item);
}

export function installQueueRequestFromItem(item: InstallItem): InstallQueueRequest {
  if (item.content) {
    return {
      kind: 'content',
      instance_id: item.content.instance_id,
      content_action: item.content.action,
    };
  }
  if (!item.loader) {
    return {
      kind: 'vanilla',
      version_id: item.versionId,
    };
  }
  return {
    kind: 'loader',
    component_id: item.loader.componentId,
    build_id: item.loader.buildId,
  };
}
