import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { api } from '../../api';
import { instances, lastInstanceId, selectedInstanceId, versionById } from '../../store';
import { toast } from '../../toast';
import type {
  BenchmarkQualificationResponse,
  BenchmarkSuiteDriverResponse,
  BenchmarkSuiteDriverStatus,
  BenchmarkSuiteDriverSuiteStatus,
  BenchmarkSuiteDriversResponse,
} from '../../types-performance';
import { Button, Pill } from '../../ui/Atoms';
import { SelectField } from '../../ui/Select';
import { errMessage } from '../../utils';
import { minecraftVersionLabel } from '../../version-display';
import { compactId, formatProofDate, labelFromToken } from './PerformanceLabFormat';
import { normalizeBenchmarkQualification, safeQualificationErrorMessage } from './PerformanceLabQualificationPreview';
import type {
  BenchmarkDriversState,
  BenchmarkMatrixState,
  BenchmarkQualificationRowChecks,
} from './PerformanceLabTypes';

const BENCHMARK_SUITE_DEFAULT_MODE = 'development';
const BENCHMARK_SUITE_DRIVER_DEFAULT_INTERVAL_SECONDS = 30;
const BENCHMARK_SUITE_DRIVER_MIN_INTERVAL_SECONDS = 5;
const BENCHMARK_SUITE_DRIVER_MAX_INTERVAL_SECONDS = 3600;

function clampBenchmarkSuiteDriverIntervalSeconds(value: number): number {
  return Math.min(
    BENCHMARK_SUITE_DRIVER_MAX_INTERVAL_SECONDS,
    Math.max(BENCHMARK_SUITE_DRIVER_MIN_INTERVAL_SECONDS, value),
  );
}

function parseBenchmarkSuiteDriverIntervalSeconds(value: string): number | null {
  const parsed = Number(value.trim());
  if (!Number.isFinite(parsed)) return null;
  return parsed;
}

function suiteProgressLabel(suite: BenchmarkSuiteDriverSuiteStatus): string {
  if (typeof suite.launched_run_count !== 'number' || typeof suite.run_count !== 'number') return 'Progress unknown';
  return `${suite.launched_run_count}/${suite.run_count} launched`;
}

function pendingRunLabel(suite: BenchmarkSuiteDriverSuiteStatus): string {
  if (typeof suite.pending_run_index !== 'number') return 'Pending none';
  return `Pending #${suite.pending_run_index + 1}`;
}

function driverUpdatedLabel(driver: BenchmarkSuiteDriverStatus): string {
  return driver.updated_at ? `Updated ${formatProofDate(driver.updated_at)}` : 'Updated unknown';
}

function preferredBenchmarkInstanceId(
  list: Array<{ id: string }>,
  selectedId: string | null,
  lastId: string | null,
): string {
  if (selectedId && list.some((instance) => instance.id === selectedId)) return selectedId;
  if (lastId && list.some((instance) => instance.id === lastId)) return lastId;
  return list[0]?.id ?? '';
}

export function BenchmarkSuiteDriversBlock({ matrixState }: { matrixState: BenchmarkMatrixState }): JSX.Element {
  const [driversState, setDriversState] = useState<BenchmarkDriversState>({ status: 'loading', data: [] });
  const [stoppingIds, setStoppingIds] = useState<Set<string>>(() => new Set());
  const [resumingIds, setResumingIds] = useState<Set<string>>(() => new Set());
  const [qualificationChecks, setQualificationChecks] = useState<BenchmarkQualificationRowChecks>({});
  const instanceRows = instances.value;
  const preferredInstanceId = preferredBenchmarkInstanceId(
    instanceRows,
    selectedInstanceId.value,
    lastInstanceId.value,
  );
  const suiteModes = useMemo(() => {
    const ids = matrixState.data?.modes.map((mode) => mode.id).filter(Boolean) ?? [];
    return ids.length > 0 ? ids : [BENCHMARK_SUITE_DEFAULT_MODE];
  }, [matrixState.data]);
  const [startInstanceId, setStartInstanceId] = useState(preferredInstanceId);
  const [startSuiteMode, setStartSuiteMode] = useState(BENCHMARK_SUITE_DEFAULT_MODE);
  const [intervalSeconds, setIntervalSeconds] = useState(String(BENCHMARK_SUITE_DRIVER_DEFAULT_INTERVAL_SECONDS));
  const [starting, setStarting] = useState(false);
  const requestRef = useRef(0);
  const qualificationRequestRef = useRef<Record<string, number>>({});
  const aliveRef = useRef(true);

  useEffect(() => {
    setStartInstanceId((current) =>
      current && instanceRows.some((instance) => instance.id === current) ? current : preferredInstanceId,
    );
  }, [instanceRows, preferredInstanceId]);

  useEffect(() => {
    setStartSuiteMode((current) => {
      if (suiteModes.includes(current)) return current;
      return suiteModes.includes(BENCHMARK_SUITE_DEFAULT_MODE) ? BENCHMARK_SUITE_DEFAULT_MODE : suiteModes[0];
    });
  }, [suiteModes]);

  const loadDrivers = async (): Promise<void> => {
    const requestId = requestRef.current + 1;
    requestRef.current = requestId;
    setDriversState((prev) => ({ status: 'loading', data: prev.data }));
    try {
      const res = await api('GET', '/launch/benchmark/suite/drivers');
      if (res?.error) throw new Error(res.error);
      if (!aliveRef.current || requestId !== requestRef.current) return;
      const drivers = (res as BenchmarkSuiteDriversResponse).drivers;
      setDriversState({ status: 'ready', data: Array.isArray(drivers) ? drivers : [] });
    } catch (err) {
      if (!aliveRef.current || requestId !== requestRef.current) return;
      setDriversState((prev) => ({ status: 'error', data: prev.data, error: errMessage(err) }));
    }
  };

  useEffect(() => {
    aliveRef.current = true;
    void loadDrivers();
    return () => {
      aliveRef.current = false;
    };
  }, []);

  const stopDriver = async (id: string): Promise<void> => {
    setStoppingIds((prev) => {
      const next = new Set(prev);
      next.add(id);
      return next;
    });
    try {
      const res = await api('POST', `/launch/benchmark/suite/drivers/${encodeURIComponent(id)}/stop`);
      if (res?.error) throw new Error(res.error);
      const nextDriver = res as BenchmarkSuiteDriverResponse;
      if (!aliveRef.current) return;
      setDriversState((prev) => ({
        status: prev.status === 'error' ? 'ready' : prev.status,
        data: prev.data.map((driver) => (driver.driver.id === id ? nextDriver : driver)),
      }));
      toast('Driver stopped');
    } catch (err) {
      if (aliveRef.current) toast(`Stop failed: ${errMessage(err)}`, 'error');
    } finally {
      if (!aliveRef.current) return;
      setStoppingIds((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    }
  };

  const resumeDriver = async (id: string): Promise<void> => {
    setResumingIds((prev) => {
      const next = new Set(prev);
      next.add(id);
      return next;
    });
    try {
      const res = await api('POST', `/launch/benchmark/suite/drivers/${encodeURIComponent(id)}/resume`);
      if (res?.error) throw new Error(res.error);
      const nextDriver = res as BenchmarkSuiteDriverResponse;
      if (!aliveRef.current) return;
      setDriversState((prev) => ({
        status: 'ready',
        data: [nextDriver, ...prev.data.filter((driver) => driver.driver.id !== nextDriver.driver.id)],
      }));
      toast('Driver resumed');
    } catch (err) {
      if (aliveRef.current) toast(`Resume failed: ${errMessage(err)}`, 'error');
    } finally {
      if (!aliveRef.current) return;
      setResumingIds((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    }
  };

  const checkFamilyCQualification = async (driverId: string, suiteId: string): Promise<void> => {
    const requestId = (qualificationRequestRef.current[driverId] ?? 0) + 1;
    qualificationRequestRef.current[driverId] = requestId;
    setQualificationChecks((prev) => ({
      ...prev,
      [driverId]: { status: 'loading', data: prev[driverId]?.data ?? null },
    }));
    try {
      const res = await api('GET', `/launch/benchmark/qualification/family-c-1-12-2/${encodeURIComponent(suiteId)}`);
      if (res?.error) throw new Error(res.error);
      if (!aliveRef.current || qualificationRequestRef.current[driverId] !== requestId) return;
      setQualificationChecks((prev) => ({
        ...prev,
        [driverId]: { status: 'ready', data: normalizeBenchmarkQualification(res as BenchmarkQualificationResponse) },
      }));
    } catch (err) {
      if (!aliveRef.current || qualificationRequestRef.current[driverId] !== requestId) return;
      setQualificationChecks((prev) => ({
        ...prev,
        [driverId]: {
          status: 'error',
          data: prev[driverId]?.data ?? null,
          error: safeQualificationErrorMessage(err),
        },
      }));
    }
  };

  const selectedStartInstance = instanceRows.find((instance) => instance.id === startInstanceId) ?? null;
  const parsedIntervalSeconds = parseBenchmarkSuiteDriverIntervalSeconds(intervalSeconds);
  const intervalValid =
    parsedIntervalSeconds !== null &&
    parsedIntervalSeconds >= BENCHMARK_SUITE_DRIVER_MIN_INTERVAL_SECONDS &&
    parsedIntervalSeconds <= BENCHMARK_SUITE_DRIVER_MAX_INTERVAL_SECONDS;
  const showIntervalError = intervalSeconds.trim().length > 0 && !intervalValid;
  const canStartDriver = Boolean(selectedStartInstance && startSuiteMode && intervalValid && !starting);

  const normalizeIntervalSeconds = (): void => {
    const parsed = parseBenchmarkSuiteDriverIntervalSeconds(intervalSeconds);
    if (parsed === null) return;
    setIntervalSeconds(String(Math.round(clampBenchmarkSuiteDriverIntervalSeconds(parsed))));
  };

  const startDriver = async (): Promise<void> => {
    if (!selectedStartInstance || !startSuiteMode || !intervalValid || parsedIntervalSeconds === null) return;
    setStarting(true);
    try {
      const intervalMs = Math.round(parsedIntervalSeconds * 1000);
      const res = await api('POST', '/launch/benchmark/suite/driver', {
        instance_id: selectedStartInstance.id,
        suite_mode: startSuiteMode,
        interval_ms: intervalMs,
      });
      if (res?.error) throw new Error(res.error);
      const nextDriver = res as BenchmarkSuiteDriverResponse;
      if (!aliveRef.current) return;
      setDriversState((prev) => ({
        status: 'ready',
        data: [nextDriver, ...prev.data.filter((driver) => driver.driver.id !== nextDriver.driver.id)],
      }));
      toast('Benchmark driver started');
    } catch (err) {
      if (aliveRef.current) toast(`Start failed: ${errMessage(err)}`, 'error');
    } finally {
      if (aliveRef.current) setStarting(false);
    }
  };

  const rows = driversState.data;

  return (
    <div class="cp-settings-driver-history" aria-live="polite">
      <div class="cp-settings-driver-history-head">
        <div class="cp-settings-rule-status-copy">
          <strong>Benchmark drivers</strong>
          <span>Recent background suite drivers for local benchmark runs.</span>
        </div>
        <div class="cp-settings-driver-actions">
          {driversState.status === 'loading' && <span class="cp-settings-proof-muted">Loading</span>}
          {driversState.status === 'error' && <span class="cp-settings-proof-error">Unavailable</span>}
          <Button
            variant="secondary"
            size="sm"
            icon="refresh"
            disabled={driversState.status === 'loading'}
            onClick={() => void loadDrivers()}
          >
            Refresh
          </Button>
        </div>
      </div>

      <div class="cp-settings-driver-start">
        <label class="cp-settings-driver-start-field cp-settings-driver-start-field--instance">
          <span>Instance</span>
          <SelectField
            value={startInstanceId}
            disabled={starting || instanceRows.length === 0}
            onChange={setStartInstanceId}
            ariaLabel="Benchmark driver instance"
            options={
              instanceRows.length === 0
                ? [{ value: '', label: 'No instances' }]
                : instanceRows.map((instance) => ({
                    value: instance.id,
                    label: `${instance.name} (${minecraftVersionLabel(
                      versionById(instance.version_id),
                      instance.version_id,
                    )})`,
                  }))
            }
          />
        </label>
        <label class="cp-settings-driver-start-field cp-settings-driver-start-field--mode">
          <span>Suite mode</span>
          <SelectField
            value={startSuiteMode}
            disabled={starting}
            onChange={setStartSuiteMode}
            ariaLabel="Benchmark suite mode"
            options={suiteModes.map((mode) => ({ value: mode, label: labelFromToken(mode, mode) }))}
          />
        </label>
        <label
          class="cp-settings-driver-start-field cp-settings-driver-start-field--interval"
          data-invalid={showIntervalError ? 'true' : 'false'}
        >
          <span>Interval</span>
          <div class="cp-settings-driver-interval-input">
            <input
              type="number"
              min={BENCHMARK_SUITE_DRIVER_MIN_INTERVAL_SECONDS}
              max={BENCHMARK_SUITE_DRIVER_MAX_INTERVAL_SECONDS}
              step={1}
              value={intervalSeconds}
              autocomplete="off"
              aria-label="Benchmark driver interval in seconds"
              aria-invalid={showIntervalError}
              disabled={starting}
              onInput={(event) => setIntervalSeconds((event.currentTarget as HTMLInputElement).value)}
              onBlur={normalizeIntervalSeconds}
            />
            <span>s</span>
          </div>
        </label>
        <Button size="sm" icon="play" sound="affirm" disabled={!canStartDriver} onClick={() => void startDriver()}>
          {starting ? 'Starting' : 'Start'}
        </Button>
      </div>
      {showIntervalError && (
        <div class="cp-settings-driver-start-error">
          Interval must be {BENCHMARK_SUITE_DRIVER_MIN_INTERVAL_SECONDS}-{BENCHMARK_SUITE_DRIVER_MAX_INTERVAL_SECONDS}{' '}
          seconds.
        </div>
      )}

      {driversState.status === 'error' && rows.length === 0 && (
        <div class="cp-settings-proof-empty">Benchmark driver status is unavailable. {driversState.error}</div>
      )}

      {driversState.status !== 'error' && rows.length === 0 && (
        <div class="cp-settings-proof-empty">
          {driversState.status === 'loading' ? 'Checking recent benchmark drivers.' : 'No recent benchmark drivers.'}
        </div>
      )}

      {rows.length > 0 && (
        <div class="cp-settings-driver-list">
          {rows.map((row) => {
            const driver = row.driver;
            const suite = row.suite;
            const rawSuiteId = suite.suite_id || driver.suite_id;
            const suiteId = rawSuiteId || 'Unknown suite';
            const mode = suite.mode || driver.mode;
            const checkState = qualificationChecks[driver.id];
            const canCheckQualification = Boolean(
              driver.id && rawSuiteId && row.view_model.can_check_family_c_qualification,
            );
            const checkingQualification = checkState?.status === 'loading';
            const canStop = Boolean(driver.id) && row.view_model.can_stop;
            const canResume = Boolean(driver.id) && row.view_model.can_resume;
            const stopping = stoppingIds.has(driver.id);
            const resuming = resumingIds.has(driver.id);

            return (
              <div class="cp-settings-driver-row" key={driver.id}>
                <div class="cp-settings-driver-main">
                  <Pill tone={row.view_model.state_tone}>{row.view_model.state_label}</Pill>
                  <div class="cp-settings-driver-title">
                    <strong title={suiteId}>{suiteId}</strong>
                    <span>{labelFromToken(mode, 'Unknown mode')}</span>
                  </div>
                </div>
                <div class="cp-settings-driver-meta">
                  <span>{suiteProgressLabel(suite)}</span>
                  <span>{pendingRunLabel(suite)}</span>
                  <span>{driverUpdatedLabel(driver)}</span>
                </div>
                <div class="cp-settings-driver-sessions">
                  {driver.active_session_id && (
                    <span title={driver.active_session_id}>Active {compactId(driver.active_session_id)}</span>
                  )}
                  {driver.last_session_id && (
                    <span title={driver.last_session_id}>Last {compactId(driver.last_session_id)}</span>
                  )}
                  {!driver.active_session_id && !driver.last_session_id && <span>No session yet</span>}
                </div>
                <div class="cp-settings-driver-control">
                  {canCheckQualification && (
                    <Button
                      variant="secondary"
                      size="sm"
                      icon="shield-check"
                      disabled={checkingQualification}
                      onClick={() => void checkFamilyCQualification(driver.id, rawSuiteId as string)}
                    >
                      {checkingQualification ? 'Checking' : 'Check'}
                    </Button>
                  )}
                  {canResume && (
                    <Button
                      variant="secondary"
                      size="sm"
                      icon="play"
                      disabled={resuming}
                      onClick={() => void resumeDriver(driver.id)}
                    >
                      {resuming ? 'Resuming' : 'Resume'}
                    </Button>
                  )}
                  {canStop && (
                    <Button
                      variant="secondary"
                      size="sm"
                      icon="stop"
                      disabled={stopping}
                      onClick={() => void stopDriver(driver.id)}
                    >
                      {stopping ? 'Stopping' : 'Stop'}
                    </Button>
                  )}
                </div>
                {checkState && (
                  <div class="cp-settings-driver-qualification">
                    {checkState.status === 'loading' && (
                      <>
                        <Pill tone="info">Checking</Pill>
                        <span>Reading Family C suite evidence.</span>
                      </>
                    )}
                    {checkState.status === 'error' && (
                      <>
                        <Pill tone="err">Unavailable</Pill>
                        <span>{checkState.error}</span>
                      </>
                    )}
                    {checkState.status === 'ready' && (
                      <>
                        <Pill tone={checkState.data.view_model.status_tone}>
                          {checkState.data.view_model.status_label}
                        </Pill>
                        <span>{checkState.data.view_model.missing_summary}</span>
                        <span>{checkState.data.view_model.suite_summary}</span>
                        <span>{checkState.data.view_model.evidence_summary}</span>
                      </>
                    )}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}

      {driversState.status === 'error' && rows.length > 0 && (
        <div class="cp-settings-proof-note">Could not refresh benchmark drivers. Showing the last loaded rows.</div>
      )}
    </div>
  );
}
