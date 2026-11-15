import type { VersionEntry } from '../generated/VersionEntry';

export function minecraftVersionLabel(version: VersionEntry | null | undefined, fallback = 'unknown'): string {
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
