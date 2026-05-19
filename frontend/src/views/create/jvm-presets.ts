export type JvmPreset = '' | 'smooth' | 'performance' | 'ultra_low_latency';

export const JVM_PRESET_ORDER: JvmPreset[] = ['', 'smooth', 'performance', 'ultra_low_latency'];

export const JVM_PRESET_LABELS: Record<JvmPreset, string> = {
  '': 'Auto',
  smooth: 'Smooth',
  performance: 'Performance',
  ultra_low_latency: 'Low latency',
};

export const JVM_PRESET_HINTS: Record<JvmPreset, string> = {
  '': 'Launcher picks the JVM flags for you.',
  smooth: 'Tuned for steady frame times.',
  performance: 'Higher throughput, hotter CPU.',
  ultra_low_latency: 'Minimise hitches at the cost of FPS.',
};
