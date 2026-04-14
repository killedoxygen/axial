type ProgressEstimateEvent = {
  phase?: string;
  current?: number;
  total?: number;
};

type EstimateSample = {
  at: number;
  pct: number;
};

type ProgressEstimatorState = {
  overallSamples: EstimateSample[];
  phase: string;
  phaseStartedAt: number;
  lastPct: number;
  lastPctAt: number;
};

type ProgressEstimatorOptions = {
  etaPhases: ReadonlySet<string>;
  minPct: number;
  maxPct: number;
  minPhaseTotal: number;
  minPhaseCurrent: number;
  minPhaseElapsedMs: number;
  minOverallElapsedMs: number;
  maxIdleMs: number;
  sampleWindowMs: number;
  minSampleWindowMs: number;
  minSamplePctDelta: number;
  minRemainingSeconds: number;
  maxRemainingSeconds: number;
};

const DEFAULT_OPTIONS: Omit<ProgressEstimatorOptions, 'etaPhases'> = {
  minPct: 10,
  maxPct: 95,
  minPhaseTotal: 4,
  minPhaseCurrent: 2,
  minPhaseElapsedMs: 4_000,
  minOverallElapsedMs: 8_000,
  maxIdleMs: 6_000,
  sampleWindowMs: 30_000,
  minSampleWindowMs: 8_000,
  minSamplePctDelta: 12,
  minRemainingSeconds: 5,
  maxRemainingSeconds: 3_600,
};

export function createProgressEstimator(
  overrides: Partial<ProgressEstimatorOptions> & Pick<ProgressEstimatorOptions, 'etaPhases'>,
): {
  formatLabel(label: string, event: ProgressEstimateEvent, pct: number, startedAt: number): string;
} {
  const options: ProgressEstimatorOptions = {
    ...DEFAULT_OPTIONS,
    ...overrides,
  };

  const state: ProgressEstimatorState = {
    overallSamples: [],
    phase: '',
    phaseStartedAt: 0,
    lastPct: 0,
    lastPctAt: 0,
  };

  return {
    formatLabel(label: string, event: ProgressEstimateEvent, pct: number, startedAt: number): string {
      const now = Date.now();
      trackProgress(state, event, pct, now, options);

      const remaining = estimateRemainingSeconds(state, event, pct, startedAt, now, options);
      if (remaining == null) return label;
      return `${label} — ${formatRemainingTime(remaining)} left`;
    },
  };
}

function trackProgress(
  state: ProgressEstimatorState,
  event: ProgressEstimateEvent,
  pct: number,
  now: number,
  options: ProgressEstimatorOptions,
): void {
  const phase = typeof event.phase === 'string' ? event.phase : '';
  if (phase !== state.phase) {
    state.phase = phase;
    state.phaseStartedAt = now;
  }

  if (pct > state.lastPct) {
    state.lastPct = pct;
    state.lastPctAt = now;
    state.overallSamples.push({ at: now, pct });
    state.overallSamples = state.overallSamples.filter((sample) => now - sample.at <= options.sampleWindowMs);
  }
}

function estimateRemainingSeconds(
  state: ProgressEstimatorState,
  event: ProgressEstimateEvent,
  pct: number,
  startedAt: number,
  now: number,
  options: ProgressEstimatorOptions,
): number | null {
  if (!options.etaPhases.has(event.phase || '')) return null;
  if (pct <= options.minPct || pct >= options.maxPct) return null;

  const total = typeof event.total === 'number' ? event.total : 0;
  const current = typeof event.current === 'number' ? event.current : 0;
  if (total < options.minPhaseTotal || current < options.minPhaseCurrent || current >= total) return null;

  if (now - state.phaseStartedAt < options.minPhaseElapsedMs) return null;
  if (now - startedAt < options.minOverallElapsedMs) return null;
  if (state.lastPctAt > 0 && now - state.lastPctAt > options.maxIdleMs) return null;

  const latest = state.overallSamples[state.overallSamples.length - 1];
  if (!latest) return null;

  let baseline: EstimateSample | null = null;
  for (let index = state.overallSamples.length - 2; index >= 0; index -= 1) {
    const sample = state.overallSamples[index];
    if (latest.at - sample.at < options.minSampleWindowMs) continue;
    if (latest.pct - sample.pct < options.minSamplePctDelta) continue;
    baseline = sample;
    break;
  }
  if (!baseline) return null;

  const pctDelta = latest.pct - baseline.pct;
  const elapsedSeconds = (latest.at - baseline.at) / 1000;
  if (pctDelta <= 0 || elapsedSeconds <= 0) return null;

  const remaining = ((100 - pct) / pctDelta) * elapsedSeconds;
  if (remaining < options.minRemainingSeconds || remaining > options.maxRemainingSeconds) return null;
  return remaining;
}

function formatRemainingTime(seconds: number): string {
  if (seconds < 90) {
    return `~${Math.max(5, Math.ceil(seconds / 5) * 5)}s`;
  }
  if (seconds < 3_600) {
    return `~${Math.ceil(seconds / 60)}m`;
  }
  return `~${Math.ceil(seconds / 3_600)}h`;
}
