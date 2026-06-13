export type JvmPreset =
  | ''
  | 'smooth'
  | 'performance'
  | 'ultra_low_latency'
  | 'graalvm'
  | 'legacy'
  | 'legacy_pvp'
  | 'legacy_heavy';

export const JVM_PRESET_ORDER: JvmPreset[] = [
  '',
  'smooth',
  'performance',
  'ultra_low_latency',
  'graalvm',
  'legacy',
  'legacy_pvp',
  'legacy_heavy',
];

export const JVM_PRESET_CREATE_ORDER: JvmPreset[] = ['', 'smooth', 'performance', 'ultra_low_latency'];

export const JVM_PRESET_LABELS: Record<JvmPreset, string> = {
  '': 'Auto',
  smooth: 'Smooth',
  performance: 'Performance',
  ultra_low_latency: 'Low latency',
  graalvm: 'GraalVM',
  legacy: 'Legacy',
  legacy_pvp: 'Legacy PvP',
  legacy_heavy: 'Legacy heavy',
};

export const JVM_PRESET_HINTS: Record<JvmPreset, string> = {
  '': 'Croopor picks safe JVM flags for this instance.',
  smooth: 'Balances throughput and steady frame times.',
  performance: 'Pushes higher throughput on modern hardware.',
  ultra_low_latency: 'Shortens JVM pauses, sometimes trading peak FPS.',
  graalvm: 'Uses flags intended for GraalVM-based Java runtimes.',
  legacy: 'Keeps conservative flags for older Minecraft and Java stacks.',
  legacy_pvp: 'Legacy tuning biased toward fast input response.',
  legacy_heavy: 'Legacy tuning for larger heaps and heavier old modpacks.',
};

export function isJvmPreset(value: string): value is JvmPreset {
  return JVM_PRESET_ORDER.includes(value as JvmPreset);
}

export function jvmPresetFrom(value: string | undefined): JvmPreset {
  const trimmed = (value ?? '').trim();
  return isJvmPreset(trimmed) ? trimmed : '';
}

export function jvmPresetLabelFor(value: string | undefined): string {
  const trimmed = (value ?? '').trim();
  if (!trimmed) return JVM_PRESET_LABELS[''];
  return isJvmPreset(trimmed) ? JVM_PRESET_LABELS[trimmed] : trimmed;
}
