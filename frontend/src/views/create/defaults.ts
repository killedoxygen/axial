import type { LoaderComponentId, Version } from '../../types';

export type LoaderKey = 'vanilla' | 'fabric' | 'quilt' | 'forge' | 'neoforge';
export type Channel = 'release' | 'snapshot' | 'legacy';

export const LOADER_KEYS: LoaderKey[] = ['vanilla', 'fabric', 'forge', 'neoforge', 'quilt'];

export const LOADER_LABELS: Record<LoaderKey, string> = {
  vanilla: 'Vanilla',
  fabric: 'Fabric',
  quilt: 'Quilt',
  forge: 'Forge',
  neoforge: 'NeoForge',
};

export const LOADER_TAGLINES: Record<LoaderKey, string> = {
  vanilla: 'Pure Minecraft, no mods',
  fabric: 'Lightweight, modern mods',
  quilt: 'Fabric-compatible, curated',
  forge: 'Classic large mods',
  neoforge: 'Forge successor, modern',
};

export const LOADER_COMPONENT_IDS: Record<Exclude<LoaderKey, 'vanilla'>, LoaderComponentId> = {
  fabric: 'net.fabricmc.fabric-loader',
  quilt: 'org.quiltmc.quilt-loader',
  forge: 'net.minecraftforge',
  neoforge: 'net.neoforged',
};

// Icons the user can pick for an instance. Every name must exist in ui/Icons.tsx REGISTRY.
export const INSTANCE_ICON_CHOICES: readonly string[] = [
  'cube',
  'compass',
  'terminal',
  'palette',
  'rectangle',
  'tag',
  'folder',
  'globe',
  'home',
  'clock',
  'music',
  'headphones',
];

const LOADER_DEFAULT_ICON: Record<LoaderKey, string> = {
  vanilla: 'cube',
  fabric: 'compass',
  quilt: 'palette',
  forge: 'terminal',
  neoforge: 'rectangle',
};

export function loaderKeyFromComponentId(componentId: LoaderComponentId | null | undefined): LoaderKey {
  if (!componentId) return 'vanilla';
  if (componentId === 'net.fabricmc.fabric-loader') return 'fabric';
  if (componentId === 'org.quiltmc.quilt-loader') return 'quilt';
  if (componentId === 'net.neoforged') return 'neoforge';
  if (componentId === 'net.minecraftforge') return 'forge';
  return 'vanilla';
}

export function loaderKeyFromVersion(version: Version | null | undefined): LoaderKey {
  if (!version?.loader) return 'vanilla';
  return loaderKeyFromComponentId(version.loader.component_id);
}

export function defaultNameFor(loader: LoaderKey, mcVersion: string): string {
  const trimmed = mcVersion.trim();
  if (!trimmed) return '';
  if (loader === 'vanilla') return trimmed;
  return `${LOADER_LABELS[loader]} ${trimmed}`;
}

export function defaultIconFor(loader: LoaderKey): string {
  return LOADER_DEFAULT_ICON[loader] ?? 'cube';
}

export function channelOf(labelOrChannel: string): Channel {
  if (labelOrChannel === 'stable') return 'release';
  if (labelOrChannel === 'preview' || labelOrChannel === 'experimental') return 'snapshot';
  return 'legacy';
}

export function channelOfVersion(version: Pick<Version, 'lifecycle'>): Channel {
  return channelOf(version.lifecycle?.channel ?? 'unknown');
}
