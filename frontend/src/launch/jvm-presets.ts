import { useEffect, useState } from 'preact/hooks';
import { createView } from '../create/api';
import type { PresetOption } from '../generated/PresetOption';

let presetCache: PresetOption[] | null = null;
let presetRequest: Promise<PresetOption[]> | null = null;

async function loadPresets(): Promise<PresetOption[]> {
  if (presetCache) return presetCache;
  presetRequest ??= (async () => {
    try {
      const list = (await createView('vanilla')).preset_options;
      if (list.length > 0) presetCache = list;
      return list;
    } catch {
      return [];
    } finally {
      presetRequest = null;
    }
  })();
  return presetRequest;
}

export function useJvmPresets(): PresetOption[] {
  const [options, setOptions] = useState<PresetOption[]>(presetCache ?? []);

  useEffect(() => {
    if (presetCache) return;
    let cancelled = false;
    void loadPresets().then((list) => {
      if (!cancelled && list.length > 0) setOptions(list);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  return options;
}

export function normalizeJvmPreset(value: string | undefined, options: PresetOption[]): string {
  const trimmed = (value ?? '').trim();
  if (options.length === 0) return trimmed;
  return options.some((option) => option.id === trimmed) ? trimmed : '';
}
