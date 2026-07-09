import { useEffect, useState } from 'preact/hooks';
import { api } from '../api';

export interface JvmPresetOption {
  id: string;
  label: string;
  detail: string;
  default: boolean;
  disabled_reason?: string | null;
}

export function useJvmPresets(): { options: JvmPresetOption[]; selectable: JvmPresetOption[] } {
  const [options, setOptions] = useState<JvmPresetOption[]>([]);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const res = (await api('GET', '/instances/create-view')) as {
          preset_options?: JvmPresetOption[];
          error?: string;
        };
        if (cancelled || res.error) return;
        const list = Array.isArray(res.preset_options)
          ? res.preset_options.filter(
              (option): option is JvmPresetOption =>
                typeof option.id === 'string' && typeof option.label === 'string' && typeof option.detail === 'string',
            )
          : [];
        setOptions(list);
      } catch {
        if (!cancelled) setOptions([]);
      }
    })();
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
