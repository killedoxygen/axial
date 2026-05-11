// Resolution presets derived from the user's largest available display.
// The Window Management API (`getScreenDetails`) reports all attached screens
// but requires a permission grant; fall back to `window.screen` for the
// primary display when it's unavailable or denied.

export interface ScreenSize { w: number; h: number; }

export interface WindowPresetSpec {
  id: string;
  label: string;
  w: number;
  h: number;
}

const CANDIDATES: WindowPresetSpec[] = [
  { id: '5k',  label: '5K',    w: 5120, h: 2880 },
  { id: '4k',  label: '4K',    w: 3840, h: 2160 },
  { id: '3k',  label: '3K',    w: 3200, h: 1800 },
  { id: '2k',  label: '2K',    w: 2560, h: 1440 },
  { id: 'fhd', label: '1080p', w: 1920, h: 1080 },
  { id: 'hd',  label: '720p',  w: 1280, h: 720 },
];

const DEFAULT_PRESET: WindowPresetSpec = { id: 'default', label: 'Default', w: 0, h: 0 };

type ScreenDetail = { width: number; height: number };
type WithScreenDetails = Window & {
  getScreenDetails?: () => Promise<{ screens: ScreenDetail[] }>;
};

export async function detectMaxScreenSize(): Promise<ScreenSize> {
  const w = window as WithScreenDetails;
  if (typeof w.getScreenDetails === 'function') {
    try {
      const details = await w.getScreenDetails();
      let maxW = 0;
      let maxH = 0;
      for (const s of details.screens) {
        if (s.width > maxW) maxW = s.width;
        if (s.height > maxH) maxH = s.height;
      }
      if (maxW > 0 && maxH > 0) return { w: maxW, h: maxH };
    } catch {
      // Permission denied or API unsupported in this surface; fall through.
    }
  }
  const sw = window.screen?.width ?? 1920;
  const sh = window.screen?.height ?? 1080;
  return { w: sw, h: sh };
}

export function buildWindowPresets(max: ScreenSize): WindowPresetSpec[] {
  const fits = CANDIDATES.filter((p) => p.w <= max.w && p.h <= max.h);
  return [...fits, DEFAULT_PRESET];
}

export function nextWindowPreset(
  presets: WindowPresetSpec[],
  currentId: string,
): WindowPresetSpec {
  if (presets.length === 0) return DEFAULT_PRESET;
  const i = presets.findIndex((p) => p.id === currentId);
  return presets[(i + 1) % presets.length]!;
}
