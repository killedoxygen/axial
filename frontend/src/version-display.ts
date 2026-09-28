import type { Version } from './types-version';

export function minecraftVersionLabel(version: Version | null | undefined, fallback = 'unknown'): string {
  if (!version) return fallback;
  const meta = version.minecraft_meta;
  return (
    version.inherits_from?.trim() ||
    meta.effective_version ||
    meta.base_id ||
    meta.display_name ||
    meta.display_hint ||
    version.id ||
    fallback
  );
}
