import type { InstallItem, LoaderComponentId } from './types';

const LOADER_LABELS: Record<LoaderComponentId, string> = {
  'net.fabricmc.fabric-loader': 'Fabric',
  'org.quiltmc.quilt-loader': 'Quilt',
  'net.minecraftforge': 'Forge',
  'net.neoforged': 'NeoForge',
};

function compact(value: string): string {
  return value.trim();
}

export function formatInstallItemLabel(item: Pick<InstallItem, 'versionId' | 'loader'>): string {
  const versionId = compact(item.versionId);
  if (!item.loader) return versionId ? `Minecraft ${versionId}` : 'Minecraft';

  const loaderName = LOADER_LABELS[item.loader.componentId];
  const loaderVersion = compact(item.loader.loaderVersion);
  const minecraftVersion = compact(item.loader.minecraftVersion);
  const label = loaderVersion ? `${loaderName} ${loaderVersion}` : `${loaderName} loader`;

  return minecraftVersion ? `${label} for Minecraft ${minecraftVersion}` : label;
}
