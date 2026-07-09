import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { api } from '../../api';
import { devMode } from '../../store';
import type { LaunchReportsResponse } from '../../types-launch';
import type { BenchmarkMatrixResponse, BenchmarkQualificationPreviewResponse } from '../../types-performance';
import { Button } from '../../ui/Atoms';
import { SettingRow } from '../../ui/SettingsSheet';
import { errMessage } from '../../utils';
import { BenchmarkMatrixBlock } from './PerformanceLabBenchmarkMatrix';
import { LaunchProofHistoryBlock } from './PerformanceLabProofHistory';
import {
  BenchmarkQualificationPreviewBlock,
  normalizeBenchmarkQualification,
} from './PerformanceLabQualificationPreview';
import { BenchmarkSuiteDriversBlock } from './PerformanceLabSuiteDrivers';
import type {
  BenchmarkMatrixState,
  BenchmarkQualificationPreviewState,
  LaunchReportsState,
} from './PerformanceLabTypes';

export function PerformanceLabCard(): JSX.Element | null {
  const isDev = devMode.value;
  const [launchReports, setLaunchReports] = useState<LaunchReportsState>({ status: 'loading', data: [] });
  const [benchmarkMatrix, setBenchmarkMatrix] = useState<BenchmarkMatrixState>({ status: 'loading', data: null });
  const [qualificationPreview, setQualificationPreview] = useState<BenchmarkQualificationPreviewState>({
    status: 'loading',
    data: null,
  });
  const [labOpen, setLabOpen] = useState(false);

  useEffect(() => {
    if (!isDev) setLabOpen(false);
  }, [isDev]);

  useEffect(() => {
    if (!isDev || !labOpen) return;
    let alive = true;
    setLaunchReports({ status: 'loading', data: [] });
    api('GET', '/launch/reports')
      .then((res) => {
        if (!alive) return;
        if (res?.error) throw new Error(res.error);
        const reports = (res as LaunchReportsResponse).reports;
        setLaunchReports({ status: 'ready', data: Array.isArray(reports) ? reports : [] });
      })
      .catch((err) => {
        if (!alive) return;
        setLaunchReports((prev) => ({ status: 'error', data: prev.data, error: errMessage(err) }));
      });
    return () => {
      alive = false;
    };
  }, [isDev, labOpen]);

  useEffect(() => {
    if (!isDev || !labOpen) return;
    let alive = true;
    setBenchmarkMatrix((prev) => ({ status: 'loading', data: prev.data }));
    api('GET', '/launch/benchmark/matrix')
      .then((res) => {
        if (!alive) return;
        if (res?.error) throw new Error(res.error);
        setBenchmarkMatrix({ status: 'ready', data: res as BenchmarkMatrixResponse });
      })
      .catch((err) => {
        if (!alive) return;
        setBenchmarkMatrix((prev) => ({ status: 'error', data: prev.data, error: errMessage(err) }));
      });
    return () => {
      alive = false;
    };
  }, [isDev, labOpen]);

  useEffect(() => {
    if (!isDev || !labOpen) return;
    let alive = true;
    setQualificationPreview((prev) => ({ status: 'loading', data: prev.data }));
    api('GET', '/launch/benchmark/qualification/family-c-1-12-2/preview')
      .then((res) => {
        if (!alive) return;
        if (res?.error) throw new Error(res.error);
        setQualificationPreview({
          status: 'ready',
          data: normalizeBenchmarkQualification(res as BenchmarkQualificationPreviewResponse),
        });
      })
      .catch((err) => {
        if (!alive) return;
        setQualificationPreview((prev) => ({ status: 'error', data: prev.data, error: errMessage(err) }));
      });
    return () => {
      alive = false;
    };
  }, [isDev, labOpen]);

  if (!isDev) return null;

  if (!labOpen) {
    return (
      <SettingRow
        title="Performance lab"
        description="Developer-only launch proof and benchmark tools."
        control={
          <Button variant="secondary" size="sm" icon="chevron-down" onClick={() => setLabOpen(true)}>
            Open
          </Button>
        }
      />
    );
  }

  return (
    <SettingRow title="Performance lab" description="Developer-only launch proof and benchmark tools.">
      <div class="cp-settings-lab-action">
        <Button variant="secondary" size="sm" icon="chevron-up" onClick={() => setLabOpen(false)}>
          Close
        </Button>
      </div>
      <LaunchProofHistoryBlock state={launchReports} />
      <BenchmarkMatrixBlock state={benchmarkMatrix} />
      <BenchmarkQualificationPreviewBlock state={qualificationPreview} />
      <BenchmarkSuiteDriversBlock matrixState={benchmarkMatrix} />
    </SettingRow>
  );
}
