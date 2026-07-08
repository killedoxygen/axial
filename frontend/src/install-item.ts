import type {
  InstallItem,
  InstallQueueInstallItemViewModel,
  InstallQueueRequest,
  InstallQueuedItemViewModel,
} from './types-install';

export function cloneInstallItem(item: InstallItem): InstallItem {
  return item.loader ? { versionId: item.versionId, loader: { ...item.loader } } : { versionId: item.versionId };
}

export function isSameInstallItem(left: InstallItem, right: InstallItem): boolean {
  if (left.versionId !== right.versionId) return false;
  if (!left.loader && !right.loader) return true;
  if (!left.loader || !right.loader) return false;
  return left.loader.componentId === right.loader.componentId && left.loader.buildId === right.loader.buildId;
}

export function installItemFromQueueInstallItem(value: InstallQueueInstallItemViewModel): InstallItem {
  const versionId = value.version_id;
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
