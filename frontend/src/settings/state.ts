import { signal } from '@preact/signals';
import type { ConfigView } from '../generated/ConfigView';
import type { SystemResourceResponse } from '../generated/SystemResourceResponse';

export const config = signal<ConfigView | null>(null);
export const systemInfo = signal<SystemResourceResponse | null>(null);

export function setConfig(c: ConfigView): boolean {
  const current = config.value;
  if (current && (c.revision < current.revision || c.account_selection_revision < current.account_selection_revision)) {
    return false;
  }
  config.value = c;
  return true;
}
