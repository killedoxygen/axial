import type { ConfigView } from '../generated/ConfigView';

/** The heap an Automatic choice aims for; the launch owner may choose less on a busy host. */
export const AUTOMATIC_MEMORY_TARGET_MB = 4096;

/** A global maximum heap of zero means Axial resolves the heap at each launch. */
export function isAutomaticMemory(cfg: Pick<ConfigView, 'max_memory_mb'> | null | undefined): boolean {
  return cfg?.max_memory_mb === 0;
}

/** The global maximum as a starting value for an explicit choice. */
export function explicitMaxMemoryMb(cfg: Pick<ConfigView, 'max_memory_mb'> | null | undefined): number {
  return cfg && cfg.max_memory_mb > 0 ? cfg.max_memory_mb : AUTOMATIC_MEMORY_TARGET_MB;
}
