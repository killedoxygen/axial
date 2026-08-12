import { useEffect, useState } from 'preact/hooks';
import { api } from '../api';
import { dtoArray, dtoBoolean, dtoOptionalString, dtoRecord, dtoString } from '../dto-contract';

export interface JvmPresetOption {
  id: string;
  label: string;
  detail: string;
  default: boolean;
  disabled_reason?: string | null;
}

let presetCache: JvmPresetOption[] | null = null;
let presetRequest: Promise<JvmPresetOption[]> | null = null;

async function loadPresets(): Promise<JvmPresetOption[]> {
  if (presetCache) return presetCache;
  presetRequest ??= (async () => {
    try {
      const response = dtoRecord(await api('GET', '/instances/create-view'), 'Create presets');
      const list = dtoArray(response.preset_options, 'Create presets').map((value): JvmPresetOption => {
        const option = dtoRecord(value, 'Create preset');
        return {
          id: dtoString(option.id, 'Create preset id'),
          label: dtoString(option.label, 'Create preset label'),
          detail: dtoString(option.detail, 'Create preset detail'),
          default: dtoBoolean(option.default, 'Create preset default'),
          disabled_reason: dtoOptionalString(option.disabled_reason, 'Create preset disabled reason'),
        };
      });
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

export function useJvmPresets(): { options: JvmPresetOption[]; selectable: JvmPresetOption[] } {
  const [options, setOptions] = useState<JvmPresetOption[]>(presetCache ?? []);

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

  return { options, selectable: options.filter((option) => !option.disabled_reason) };
}

export function normalizeJvmPreset(value: string | undefined, selectable: JvmPresetOption[]): string {
  const trimmed = (value ?? '').trim();
  if (selectable.length === 0) return trimmed;
  return selectable.some((option) => option.id === trimmed) ? trimmed : '';
}

export function jvmPresetSelectLabel(option: JvmPresetOption): string {
  return option.disabled_reason ? `${option.label} (${option.disabled_reason})` : option.label;
}
