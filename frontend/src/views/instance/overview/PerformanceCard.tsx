import type { JSX } from 'preact';
import { useCallback, useEffect, useRef, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Card } from '../../../ui/Atoms';
import { api } from '../../../api';
import { config, systemInfo, versions } from '../../../store';
import { errMessage } from '../../../utils';
import { loaderKeyFromVersion } from '../../create/defaults';
import type {
  CompositionTier,
  EnrichedInstance,
  PerformanceHealthResponse,
  PerformanceHealthStatus,
  PerformanceInstanceOperationResponse,
  PerformanceMode,
  PerformanceOperationStatus,
  PerformancePlanResponse,
  Version,
} from '../../../types';
import { memoryGb } from '../format';
import { globalPerformanceMode, performanceModeFrom, performanceModeLabel } from '../performance-mode';

type PerformanceProgramState =
  | { status: 'loading'; plan: PerformancePlanResponse | null; health: PerformanceHealthResponse | null; error?: undefined }
  | { status: 'ready'; plan: PerformancePlanResponse | null; health: PerformanceHealthResponse | null; error?: undefined }
  | { status: 'error'; plan: PerformancePlanResponse | null; health: PerformanceHealthResponse | null; error: string };

interface PerformanceInstallProgress {
  phase?: string;
  current?: number;
  total?: number;
  file?: string;
  error?: string;
  done?: boolean;
}

function effectivePerformanceMode(inst: EnrichedInstance): { mode: PerformanceMode; source: 'instance' | 'global' } {
  const instanceMode = performanceModeFrom(inst.performance_mode);
  if (instanceMode) return { mode: instanceMode, source: 'instance' };
  return { mode: globalPerformanceMode(), source: 'global' };
}

function compositionTierLabel(tier: CompositionTier | ''): string {
  if (tier === 'extended') return 'Extended';
  if (tier === 'core') return 'Core';
  if (tier === 'vanilla_enhanced') return 'Vanilla enhanced';
  return 'Managed';
}

function healthLabel(health: PerformanceHealthStatus | undefined): string {
  if (health === 'healthy') return 'healthy';
  if (health === 'degraded') return 'degraded';
  if (health === 'fallback') return 'fallback';
  if (health === 'invalid') return 'needs attention';
  if (health === 'disabled') return 'not installed';
  return 'unknown';
}

function healthTone(health: PerformanceHealthStatus | undefined): 'ok' | 'warn' | 'err' | 'mute' {
  if (health === 'healthy') return 'ok';
  if (health === 'degraded' || health === 'fallback' || health === 'disabled') return 'warn';
  if (health === 'invalid') return 'err';
  return 'mute';
}

function planLoader(v: Version | undefined, inst: EnrichedInstance): string {
  const typedLoader = loaderKeyFromVersion(v);
  if (typedLoader !== 'vanilla') return typedLoader;
  const raw = inst.version_id.toLowerCase();
  if (raw.includes('neoforge')) return 'neoforge';
  if (raw.includes('fabric')) return 'fabric';
  if (raw.includes('forge')) return 'forge';
  if (raw.includes('quilt')) return 'quilt';
  return 'vanilla';
}

function planGameVersion(v: Version | undefined, inst: EnrichedInstance): string {
  return v?.minecraft_meta.effective_version
    || v?.minecraft_meta.base_id
    || v?.minecraft_meta.display_name
    || inst.version_id;
}

function performanceSummary(
  state: PerformanceProgramState,
  mode: PerformanceMode,
): { tone: 'ok' | 'warn' | 'err' | 'mute'; title: string; detail: string } {
  if (state.status === 'loading' && !state.plan && !state.health) {
    return {
      tone: 'mute',
      title: 'Checking plan',
      detail: 'Memory and Java evidence stays visible while Croopor reads bundle state.',
    };
  }
  if (state.status === 'error' && !state.plan && !state.health) {
    return {
      tone: 'mute',
      title: 'Plan status unavailable',
      detail: 'Backend plan data is not available right now.',
    };
  }
  if (mode === 'vanilla') {
    return {
      tone: 'mute',
      title: 'No managed bundle',
      detail: 'Memory allocation and Java detection are shown below.',
    };
  }
  if (mode === 'custom') {
    return {
      tone: 'mute',
      title: 'No managed bundle',
      detail: 'Memory allocation and Java detection are shown below.',
    };
  }

  const plan = state.plan;
  const health = state.health;
  if (!plan) {
    return {
      tone: 'mute',
      title: 'Bundle status unavailable',
      detail: 'Plan details are unavailable.',
    };
  }

  const tier = compositionTierLabel(plan.tier);
  const modCount = plan.mods?.length ?? 0;
  const composition = plan.composition_id ? `Composition ${plan.composition_id}` : 'No managed composition selected';
  const healthText = health ? `bundle ${healthLabel(health.health)}` : 'health not checked';
  const warning = health?.warnings?.[0] || plan.warnings?.[0] || plan.fallback_reason || '';

  if (health?.health === 'fallback') {
    const fallbackTier = health.tier ? compositionTierLabel(health.tier) : 'Managed';
    return {
      tone: healthTone(health.health),
      title: `${fallbackTier} fallback`,
      detail: warning || `Croopor safely lowered the requested ${tier} plan.`,
    };
  }

  return {
    tone: healthTone(health?.health),
    title: `${tier} plan`,
    detail: warning || `${composition}, ${modCount} managed mod${modCount === 1 ? '' : 's'}, ${healthText}.`,
  };
}

function performanceSummaryIcon(tone: 'ok' | 'warn' | 'err' | 'mute'): string {
  if (tone === 'ok') return 'check-circle';
  if (tone === 'warn' || tone === 'err') return 'alert';
  return 'info';
}

function performanceProgressTitle(progress: PerformanceInstallProgress): string {
  if (progress.phase === 'queued') return 'Bundle queued';
  if (progress.phase === 'planning') return 'Planning bundle';
  if (progress.phase === 'applying') return 'Applying bundle';
  if (progress.phase === 'removing') return 'Removing bundle';
  if (progress.phase === 'rolling_back') return 'Rolling back bundle';
  if (progress.phase === 'complete') return 'Bundle updated';
  if (progress.phase === 'error') return 'Bundle update failed';
  return 'Updating bundle';
}

function performanceProgressDetail(progress: PerformanceInstallProgress): string {
  if (progress.error) return progress.error;
  if (progress.file?.trim()) return progress.file;
  if (progress.phase === 'queued') return 'Waiting to update managed performance files.';
  if (progress.phase === 'planning') return 'Checking the managed performance plan.';
  if (progress.phase === 'applying') return 'Applying managed performance files.';
  if (progress.phase === 'removing') return 'Removing managed performance files.';
  if (progress.phase === 'rolling_back') return 'Rolling back managed performance files.';
  if (progress.phase === 'complete') return 'Managed performance update complete.';
  if (progress.phase === 'error') return 'Performance update failed.';
  return 'Updating managed performance files.';
}

function isPerformanceOperationTerminal(status: PerformanceOperationStatus): boolean {
  return status.state === 'complete' || status.state === 'failed' || status.state === 'interrupted';
}

function isPerformanceOperationComplete(status: PerformanceOperationStatus): boolean {
  return status.state === 'complete';
}

function operationStatusAsProgress(status: PerformanceOperationStatus): PerformanceInstallProgress {
  const failed = status.state === 'failed' || status.state === 'interrupted';
  const phase = failed ? 'error' : status.state;
  const current = phase === 'queued'
    ? 0
    : phase === 'planning'
      ? 1
      : phase === 'complete' || phase === 'error'
        ? 4
        : 2;
  return {
    phase,
    current,
    total: 4,
    error: failed ? status.error || 'performance operation failed' : status.error,
    done: isPerformanceOperationTerminal(status),
  };
}

function fmtHeap(gb: number): string {
  return Number.isInteger(gb) ? String(gb) : gb.toFixed(1);
}

function heapLabel(minGb: number, maxGb: number): string {
  return minGb === maxGb ? `${fmtHeap(maxGb)} GB` : `${fmtHeap(minGb)} to ${fmtHeap(maxGb)} GB`;
}

function MemoryBar({ minGb, maxGb, totalGb }: { minGb: number; maxGb: number; totalGb: number }): JSX.Element {
  const clampFrac = (v: number): number => (Number.isFinite(v) ? Math.max(0, Math.min(1, v)) : 0);
  const maxFrac = clampFrac(maxGb / totalGb);
  const minFrac = clampFrac(minGb / totalGb);
  const sameHeap = minGb === maxGb;
  return (
    <div
      class="cp-od-perf-mem"
      role="img"
      aria-label={`Heap ${sameHeap ? fmtHeap(maxGb) : `${fmtHeap(minGb)} to ${fmtHeap(maxGb)}`} GB of ${fmtHeap(totalGb)} GB system memory`}
    >
      <div class="cp-od-perf-mem-track">
        <span class="cp-od-perf-mem-fill cp-od-perf-mem-fill--max" style={{ width: `${maxFrac * 100}%` }} />
        <span class="cp-od-perf-mem-fill cp-od-perf-mem-fill--min" style={{ width: `${minFrac * 100}%` }} />
      </div>
      <div class="cp-od-perf-mem-scale" aria-hidden="true">
        {!sameHeap && (
          <span class="cp-od-perf-mem-mark cp-od-perf-mem-mark--min" style={{ left: `${minFrac * 100}%` }}>
            {fmtHeap(minGb)} GB
          </span>
        )}
        <span class="cp-od-perf-mem-mark cp-od-perf-mem-mark--max" style={{ left: `${maxFrac * 100}%` }}>
          {fmtHeap(maxGb)} GB
        </span>
        <span class="cp-od-perf-mem-mark cp-od-perf-mem-mark--total">{fmtHeap(totalGb)} GB</span>
      </div>
    </div>
  );
}

export function PerformanceCard({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const version = versions.value.find(v => v.id === inst.version_id);
  const effectiveMode = effectivePerformanceMode(inst);
  const maxMem = memoryGb(inst.max_memory_mb, config.value?.max_memory_mb ?? 4096);
  const minMem = memoryGb(inst.min_memory_mb, config.value?.min_memory_mb ?? 1024);
  const [program, setProgram] = useState<PerformanceProgramState>({ status: 'loading', plan: null, health: null });
  const [lifecycleOperation, setLifecycleOperation] = useState<PerformanceOperationStatus | null>(null);
  const operationPollRef = useRef<number | null>(null);
  const operationRequestRef = useRef(0);

  const fetchPerformanceProgram = useCallback(async (): Promise<{
    plan: PerformancePlanResponse | null;
    health: PerformanceHealthResponse | null;
  }> => {
    const gameVersion = planGameVersion(version, inst);
    const loader = planLoader(version, inst);
    const planParams = new URLSearchParams({
      game_version: gameVersion,
      loader,
      mode: effectiveMode.mode,
      instance_id: inst.id,
    });
    const healthParams = new URLSearchParams({ instance_id: inst.id });
    const [planRes, healthRes]: [any, any] = await Promise.all([
      api('GET', `/performance/plan?${planParams.toString()}`),
      api('GET', `/performance/health?${healthParams.toString()}`),
    ]);
    if (planRes?.error) throw new Error(planRes.error);
    if (healthRes?.error) throw new Error(healthRes.error);
    return {
      plan: planRes?.mode ? planRes as PerformancePlanResponse : null,
      health: healthRes?.health ? healthRes as PerformanceHealthResponse : null,
    };
  }, [inst.id, inst.version_id, version?.id, version?.loader?.component_id, version?.minecraft_meta.effective_version, effectiveMode.mode]);

  useEffect(() => {
    return () => {
      if (operationPollRef.current !== null) window.clearInterval(operationPollRef.current);
    };
  }, []);

  useEffect(() => {
    let alive = true;
    setProgram(current => ({ status: 'loading', plan: current.plan, health: current.health }));
    void fetchPerformanceProgram()
      .then(({ plan, health }) => {
        if (!alive) return;
        setProgram({
          status: 'ready',
          plan,
          health,
        });
      })
      .catch((err) => {
        if (!alive) return;
        setProgram(current => ({
          status: 'error',
          plan: current.plan,
          health: current.health,
          error: errMessage(err),
        }));
      });

    return () => { alive = false; };
  }, [fetchPerformanceProgram]);

  useEffect(() => {
    let alive = true;
    const requestId = operationRequestRef.current + 1;
    operationRequestRef.current = requestId;
    if (operationPollRef.current !== null) {
      window.clearInterval(operationPollRef.current);
      operationPollRef.current = null;
    }

    const applyStatus = (status: PerformanceOperationStatus | null): boolean => {
      if (!alive || requestId !== operationRequestRef.current) return true;
      if (status && isPerformanceOperationComplete(status)) {
        setLifecycleOperation(null);
        return true;
      }
      setLifecycleOperation(status);
      return !status || isPerformanceOperationTerminal(status);
    };

    const refreshAfterComplete = async (): Promise<void> => {
      const refreshed = await fetchPerformanceProgram();
      if (alive && requestId === operationRequestRef.current) {
        setProgram({ status: 'ready', ...refreshed });
      }
    };

    const pollStatus = async (operationId: string): Promise<void> => {
      try {
        const res: any = await api(
          'GET',
          `/performance/operations/${encodeURIComponent(operationId)}`,
        );
        if (!res?.id && res?.error) throw new Error(res.error);
        const status = res as PerformanceOperationStatus;
        const terminal = applyStatus(status);
        if (terminal && operationPollRef.current !== null) {
          window.clearInterval(operationPollRef.current);
          operationPollRef.current = null;
        }
        if (terminal && isPerformanceOperationComplete(status)) {
          await refreshAfterComplete();
        }
      } catch {
        if (alive && requestId === operationRequestRef.current) {
          applyStatus(null);
          if (operationPollRef.current !== null) {
            window.clearInterval(operationPollRef.current);
            operationPollRef.current = null;
          }
        }
      }
    };

    void (async () => {
      try {
        const res: PerformanceInstanceOperationResponse & { error?: string } = await api(
          'GET',
          `/performance/instances/${encodeURIComponent(inst.id)}/operation`,
        );
        if (res?.error) throw new Error(res.error);
        const operation = res.operation ?? null;
        const terminal = applyStatus(operation);
        if (operation && isPerformanceOperationComplete(operation)) {
          await refreshAfterComplete();
          return;
        }
        if (operation && !terminal) {
          operationPollRef.current = window.setInterval(() => {
            void pollStatus(operation.id);
          }, 1250);
          void pollStatus(operation.id);
        }
      } catch {
        applyStatus(null);
      }
    })();

    return () => {
      alive = false;
      if (operationPollRef.current !== null) {
        window.clearInterval(operationPollRef.current);
        operationPollRef.current = null;
      }
    };
  }, [inst.id, fetchPerformanceProgram]);

  const baseSummary = performanceSummary(program, effectiveMode.mode);
  const operationProgress = lifecycleOperation ? operationStatusAsProgress(lifecycleOperation) : null;
  const visibleLifecycleProgress = operationProgress
    ? {
      title: performanceProgressTitle(operationProgress),
      detail: performanceProgressDetail(operationProgress),
    }
    : null;
  const summary = visibleLifecycleProgress
    ? {
      tone: operationProgress?.phase === 'error'
        ? 'err' as const
        : operationProgress?.done
          ? 'ok' as const
          : 'mute' as const,
      title: visibleLifecycleProgress.title || 'Updating bundle',
      detail: visibleLifecycleProgress.detail || 'Croopor is updating managed performance files.',
    }
    : baseSummary;
  const summaryIcon = performanceSummaryIcon(summary.tone);
  const runtimeDetected = Boolean(inst.java_major);
  const totalGb = systemInfo.value?.total_memory_mb
    ? Math.max(1, Math.round(systemInfo.value.total_memory_mb / 1024))
    : 32;

  return (
    <Card padding={18}>
      <div class="cp-od-head">
        <h3>Performance</h3>
      </div>

      <div class="cp-od-perf-body">
        <div class="cp-od-perf-summary" data-tone={summary.tone} aria-live="polite">
          <span class="cp-od-perf-summary-mark">
            <Icon name={summaryIcon} size={16} stroke={2.4} />
          </span>
          <div class="cp-od-perf-summary-copy">
            <strong>{summary.title}</strong>
            <span>{summary.detail}</span>
          </div>
        </div>

        <div class="cp-od-perf-meter">
          <div class="cp-od-perf-meter-head">
            <span>Memory allocation</span>
            <strong>{heapLabel(minMem, maxMem)}</strong>
          </div>
          <MemoryBar minGb={minMem} maxGb={maxMem} totalGb={totalGb} />
          <div class="cp-od-perf-footer">
            <div class="cp-od-perf-runtime" data-detected={runtimeDetected}>
              <span class="cp-od-perf-runtime-mark"><Icon name="check" size={11} stroke={2.8} /></span>
              <span class="cp-od-perf-runtime-text">{runtimeDetected ? `Java ${inst.java_major}` : 'Managed Java'}</span>
            </div>
            <span class="cp-od-perf-footer-mode">
              {performanceModeLabel(effectiveMode.mode)}
              <span class="cp-od-perf-footer-sep">·</span>
              {effectiveMode.source === 'instance' ? 'Per instance' : 'Global default'}
            </span>
          </div>
        </div>
      </div>
    </Card>
  );
}
