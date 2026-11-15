import { batch, computed, signal } from '@preact/signals';
import type { VersionEntry } from '../generated/VersionEntry';
import type { EnrichedInstance } from '../generated/EnrichedInstance';

export const instances = signal<EnrichedInstance[]>([]);
export const versions = signal<VersionEntry[]>([]);
export const lastInstanceId = signal<string | null>(null);
export const selectedInstanceId = signal<string | null>(null);

export const selectedInstance = computed<EnrichedInstance | null>(() => {
  const id = selectedInstanceId.value;
  if (!id) return null;
  return instances.value.find((i) => i.id === id) ?? null;
});

export function versionById(id: string | undefined): VersionEntry | undefined {
  if (!id) return undefined;
  return versions.value.find((v) => v.id === id);
}

export function selectInstance(id: string | null): void {
  selectedInstanceId.value = id;
}

export function addInstance(inst: EnrichedInstance): void {
  instances.value = [...instances.value, inst];
}

export function removeInstance(id: string): void {
  batch(() => {
    instances.value = instances.value.filter((i) => i.id !== id);
    if (selectedInstanceId.value === id) selectedInstanceId.value = null;
  });
}

export function updateInstanceInList(updated: EnrichedInstance): void {
  instances.value = instances.value.map((i) => (i.id === updated.id && updated.revision >= i.revision ? updated : i));
}
