import { request } from '../platform/transport';
import type { BenchmarkDriverView } from '../generated/BenchmarkDriverView';
import type { BenchmarkDriversResponse } from '../generated/BenchmarkDriversResponse';
import type { BenchmarkLaunchRequest } from '../generated/BenchmarkLaunchRequest';
import type { BenchmarkMatrix } from '../generated/BenchmarkMatrix';
import type { LaunchProofView } from '../generated/LaunchProofView';
import type { LaunchReportsResponse } from '../generated/LaunchReportsResponse';
import type { QualificationResponse } from '../generated/QualificationResponse';

const benchmark = '/launch/benchmark';
const driver = (id: string): string => `${benchmark}/suite/drivers/${encodeURIComponent(id)}`;

export function launchReports(): Promise<LaunchReportsResponse> {
  return request('GET', '/launch/reports');
}

export function launchReport(sessionId: string): Promise<LaunchProofView> {
  return request('GET', `/launch/reports/${encodeURIComponent(sessionId)}`);
}

export function benchmarkMatrix(): Promise<BenchmarkMatrix> {
  return request('GET', `${benchmark}/matrix`);
}

export function benchmarkDrivers(): Promise<BenchmarkDriversResponse> {
  return request('GET', `${benchmark}/suite/drivers`);
}

export function startBenchmarkDriver(body: BenchmarkLaunchRequest): Promise<BenchmarkDriverView> {
  return request('POST', `${benchmark}/suite/driver`, body);
}

export function stopBenchmarkDriver(id: string): Promise<BenchmarkDriverView> {
  return request('POST', `${driver(id)}/stop`);
}

export function resumeBenchmarkDriver(id: string): Promise<BenchmarkDriverView> {
  return request('POST', `${driver(id)}/resume`);
}

export function qualificationPreview(): Promise<QualificationResponse> {
  return request('GET', `${benchmark}/qualification/family-c-1-12-2/preview`);
}

export function qualification(suiteId: string): Promise<QualificationResponse> {
  return request('GET', `${benchmark}/qualification/family-c-1-12-2/${encodeURIComponent(suiteId)}`);
}
