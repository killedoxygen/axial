import type { SelectFieldOption } from '../../ui/Select';
import type { ContentKind, ContentSort } from '../../types-content';
import type { Version } from '../../types-version';
import { compareMcDesc } from './shared';

export const SORT_OPTIONS: SelectFieldOption<ContentSort>[] = [
  { value: 'relevance', label: 'Relevance' },
  { value: 'downloads', label: 'Most downloads' },
  { value: 'follows', label: 'Most followed' },
  { value: 'updated', label: 'Recently updated' },
  { value: 'newest', label: 'Newest' },
];

export const LOADER_OPTIONS: SelectFieldOption<string>[] = [
  { value: '', label: 'Any loader' },
  { value: 'fabric', label: 'Fabric' },
  { value: 'forge', label: 'Forge' },
  { value: 'neoforge', label: 'NeoForge' },
  { value: 'quilt', label: 'Quilt' },
];

export const KIND_CATEGORIES: Record<ContentKind, string[]> = {
  mod: [
    'adventure',
    'decoration',
    'economy',
    'equipment',
    'food',
    'game-mechanics',
    'library',
    'magic',
    'management',
    'minigame',
    'mobs',
    'optimization',
    'social',
    'storage',
    'technology',
    'transportation',
    'utility',
    'worldgen',
  ],
  modpack: [
    'adventure',
    'challenging',
    'combat',
    'kitchen-sink',
    'lightweight',
    'magic',
    'multiplayer',
    'optimization',
    'quests',
    'technology',
  ],
  resource_pack: [
    'combat',
    'decoration',
    'modded',
    'realistic',
    'simplistic',
    'themed',
    'tweaks',
    'utility',
    'vanilla-like',
  ],
  shader_pack: [
    'atmosphere',
    'bloom',
    'colored-lighting',
    'foliage',
    'pbr',
    'reflections',
    'shadows',
    'potato',
    'low',
    'medium',
    'high',
  ],
};

const RECENT_MC = ['1.21.6', '1.21.5', '1.21.4', '1.21.3', '1.21.1', '1.21', '1.20.6', '1.20.4', '1.20.1'];

/** Every release the launcher knows, newest first, under "Any version". */
export function gameVersionOptions(known: Version[]): SelectFieldOption<string>[] {
  const ids = new Set<string>(RECENT_MC);
  for (const version of known) {
    if (!version.loader && /^\d+\.\d+(\.\d+)?$/.test(version.id)) ids.add(version.id);
  }
  return [
    { value: '', label: 'Any version' },
    ...[...ids].sort(compareMcDesc).map((value) => ({ value, label: value })),
  ];
}
