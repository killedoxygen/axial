import type { JSX } from 'preact';
import { useCallback, useEffect, useRef, useState } from 'preact/hooks';
import { Icon } from '../../../ui/Icons';
import { Card } from '../../../ui/Atoms';
import { api } from '../../../api';
import { systemInfo } from '../../../store';
import { errMessage } from '../../../utils';
import type {
  ApplicationViewModelTone,
  EnrichedInstance,
  PerformanceHealthResponse,
  PerformanceInstanceOperationResponse,
  PerformanceOperationStatus,
} from '../../../types';

type PerformanceProgramState =
  | {
      status: 'loading';
      health: PerformanceHealthResponse | null;
      error?: undefined;
    }
  | {
      status: 'ready';
      health: PerformanceHealthResponse | null;
      error?: undefined;
    }
  | { status: 'error'; health: PerformanceHealthResponse | null; error: string };

interface PerformanceInstallProgress {
  phase?: string;
  current?: number;
  total?: number;
  file?: string;
  error?: string;
  done?: boolean;
}

type ApiResult<T> = T & { error?: string };

function performanceSummary(state: PerformanceProgramState): {
  tone: ApplicationViewModelTone;
  title: string;
  detail: string;
} {
  if (state.status === 'loading' && !state.health) {
    return {
      tone: 'mute',
      title: 'Checking plan',
      detail: 'Memory and Java evidence stays visible while Croopor reads bundle state.',
    };
  }
  if (state.status === 'error' && !state.health) {
    return {
      tone: 'mute',
      title: 'Plan status unavailable',
      detail: 'Backend plan data is not available right now.',
    };
  }
  const viewModel = state.health?.view_model;
  if (!viewModel) {
    return {
      tone: 'mute',
      title: 'Plan status unavailable',
      detail: 'Backend plan data is not available right now.',
    };
  }
  return {
    tone: viewModel.tone,
    title: viewModel.title,
    detail: viewModel.detail,
  };
}

function performanceSummaryIcon(tone: ApplicationViewModelTone): string {
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
  const current = phase === 'queued' ? 0 : phase === 'planning' ? 1 : phase === 'complete' || phase === 'error' ? 4 : 2;
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
  const [program, setProgram] = useState<PerformanceProgramState>({ status: 'loading', health: null });
  const [lifecycleOperation, setLifecycleOperation] = useState<PerformanceOperationStatus | null>(null);
  const operationPollRef = useRef<number | null>(null);
  const operationRequestRef = useRef(0);

  const fetchPerformanceProgram = useCallback(async (): Promise<{
    health: PerformanceHealthResponse | null;
  }> => {
    const healthParams = new URLSearchParams({ instance_id: inst.id });
    const healthRes = await api<ApiResult<PerformanceHealthResponse>>(
      'GET',
      `/performance/health?${healthParams.toString()}`,
    );
    if (healthRes?.error) throw new Error(healthRes.error);
    return {
      health: healthRes?.health ? (healthRes as PerformanceHealthResponse) : null,
    };
  }, [inst.id]);

  useEffect(() => {
    return () => {
      if (operationPollRef.current !== null) window.clearInterval(operationPollRef.current);
    };
  }, []);

  useEffect(() => {
    let alive = true;
    setProgram((current) => ({ status: 'loading', health: current.health }));
    void fetchPerformanceProgram()
      .then(({ health }) => {
        if (!alive) return;
        setProgram({
          status: 'ready',
          health,
        });
      })
      .catch((err) => {
        if (!alive) return;
        setProgram((current) => ({
          status: 'error',
          health: current.health,
          error: errMessage(err),
        }));
      });

    return () => {
      alive = false;
    };
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
        const res = await api<ApiResult<PerformanceOperationStatus>>(
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
        const res = await api<ApiResult<PerformanceInstanceOperationResponse>>(
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

  const baseSummary = performanceSummary(program);
  const operationProgress = lifecycleOperation ? operationStatusAsProgress(lifecycleOperation) : null;
  const visibleLifecycleProgress = operationProgress
    ? {
        title: performanceProgressTitle(operationProgress),
        detail: performanceProgressDetail(operationProgress),
      }
    : null;
  const summary = visibleLifecycleProgress
    ? {
        tone:
          operationProgress?.phase === 'error'
            ? ('err' as const)
            : operationProgress?.done
              ? ('ok' as const)
              : ('mute' as const),
        title: visibleLifecycleProgress.title || 'Updating bundle',
        detail: visibleLifecycleProgress.detail || 'Croopor is updating managed performance files.',
      }
    : baseSummary;
  const summaryIcon = performanceSummaryIcon(summary.tone);
  const display = program.health?.display ?? null;
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
            <strong>{display?.memory.label || 'Checking allocation'}</strong>
          </div>
          {display ? (
            <MemoryBar minGb={display.memory.min_gb} maxGb={display.memory.max_gb} totalGb={totalGb} />
          ) : (
            <MemoryBar minGb={0} maxGb={0} totalGb={totalGb} />
          )}
          <div class="cp-od-perf-footer">
            <div class="cp-od-perf-runtime" data-detected={display?.runtime.detected ?? false}>
              <span class="cp-od-perf-runtime-mark">
                <Icon name="check" size={11} stroke={2.8} />
              </span>
              <span class="cp-od-perf-runtime-text">{display?.runtime.label || 'Checking runtime'}</span>
            </div>
            <span class="cp-od-perf-footer-mode">
              {display?.mode.label || 'Checking mode'}
              <span class="cp-od-perf-footer-sep">·</span>
              {display?.mode.source_label || 'Backend status'}
            </span>
          </div>
        </div>
      </div>
    </Card>
  );
}
