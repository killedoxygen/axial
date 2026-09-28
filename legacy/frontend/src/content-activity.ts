import { signal } from '@preact/signals';

export const contentRevision = signal(0);

export function markContentChanged(): void {
  contentRevision.value += 1;
}
