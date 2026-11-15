import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import {
  benchmarkMatrix as readBenchmarkMatrix,
  launchReports as readLaunchReports,
  qualificationPreview as readQualificationPreview,
} from './api';
import { devMode } from '../app/store';
import { Button } from '../ui/Atoms';
import { SettingRow } from '../ui/SettingsSheet';
import { errMessage } from '../app/toast';
import { BenchmarkMatrixBlock } from './BenchmarkMatrix';
import { LaunchProofHistoryBlock } from './ProofHistory';
import { BenchmarkQualificationPreviewBlock } from './QualificationPreview';
import { BenchmarkSuiteDriversBlock } from './SuiteDrivers';
import type { BenchmarkMatrixState, BenchmarkQualificationPreviewState, LaunchReportsState } from './types';

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
    readLaunchReports()
      .then((res) => {
        if (!alive) return;
        setLaunchReports({ status: 'ready', data: res.reports });
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
    readBenchmarkMatrix()
      .then((res) => {
        if (!alive) return;
        setBenchmarkMatrix({ status: 'ready', data: res });
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
    readQualificationPreview()
      .then((res) => {
        if (!alive) return;
        setQualificationPreview({
          status: 'ready',
          data: res,
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

  return (
    <SettingRow
      title="Performance lab"
      description="Developer-only launch proof and benchmark tools."
      control={
        <Button variant="secondary" onClick={() => setLabOpen(!labOpen)}>
          {labOpen ? 'Close' : 'Open'}
        </Button>
      }
    >
      {labOpen && (
        <>
          <LaunchProofHistoryBlock state={launchReports} />
          <BenchmarkMatrixBlock state={benchmarkMatrix} />
          <BenchmarkQualificationPreviewBlock state={qualificationPreview} />
          <BenchmarkSuiteDriversBlock matrixState={benchmarkMatrix} />
        </>
      )}
    </SettingRow>
  );
}
