import type { ContentUpdate, InstanceContentEntry } from '../../types-content';

export interface ModProvenance {
  entries: Map<string, InstanceContentEntry>;
  updates: Map<string, ContentUpdate>;
  updateError?: string;
}

const provenanceCache = new Map<string, ModProvenance>();
const provenanceGenerations = new Map<string, number>();
let nextGeneration = 0;

export function cachedModProvenance(instanceId: string): ModProvenance | null {
  return provenanceCache.get(instanceId) ?? null;
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
  provenanceCache.set(instanceId, provenance);
}

export function clearModProvenance(instanceId: string): void {
  provenanceCache.delete(instanceId);
  provenanceGenerations.delete(instanceId);
}
