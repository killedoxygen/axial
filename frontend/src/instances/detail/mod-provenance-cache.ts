import { signal } from '@preact/signals';
import type { AvailableUpdate } from '../../generated/AvailableUpdate';
import type { InstalledEntry } from '../../generated/InstalledEntry';
import type { InstalledPack } from '../../generated/InstalledPack';

export interface ModProvenance {
  /** Managed mods by filename. */
  entries: Map<string, InstalledEntry>;
  /** Managed content of every kind, for the resource and shader pack panes. */
  managed: InstalledEntry[];
  /** Modpacks this instance came from. */
  packs: InstalledPack[];
  /** Available updates for every managed kind, by canonical id. */
  updates: Map<string, AvailableUpdate>;
  updateError?: string;
}

// A signal, so the instance header's pack chips follow whichever pane last read provenance.
const provenanceCache = signal<ReadonlyMap<string, ModProvenance>>(new Map());
const provenanceGenerations = new Map<string, number>();
let nextGeneration = 0;

export function cachedModProvenance(instanceId: string): ModProvenance | null {
  return provenanceCache.value.get(instanceId) ?? null;
}

export function beginModProvenanceRefresh(instanceId: string): number {
  const generation = ++nextGeneration;
  provenanceGenerations.set(instanceId, generation);
  return generation;
}

export function isCurrentModProvenanceRefresh(instanceId: string, generation: number): boolean {
  return provenanceGenerations.get(instanceId) === generation;
}

export function cacheModProvenance(instanceId: string, provenance: ModProvenance): void {
  provenanceCache.value = new Map(provenanceCache.value).set(instanceId, provenance);
}

export function clearModProvenance(instanceId: string): void {
  const next = new Map(provenanceCache.value);
  next.delete(instanceId);
  provenanceCache.value = next;
  provenanceGenerations.delete(instanceId);
}
