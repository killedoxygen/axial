import type { LaunchProofRecord } from '../../types-launch';
import type {
  BenchmarkMatrixResponse,
  BenchmarkQualificationPreviewResponse,
  BenchmarkQualificationResponse,
  BenchmarkSuiteDriverResponse,
} from '../../types-performance';

export type LaunchReportsState =
  | { status: 'loading'; data: LaunchProofRecord[]; error?: undefined }
  | { status: 'ready'; data: LaunchProofRecord[]; error?: undefined }
  | { status: 'error'; data: LaunchProofRecord[]; error: string };

export type BenchmarkMatrixState =
  | { status: 'loading'; data: BenchmarkMatrixResponse | null; error?: undefined }
  | { status: 'ready'; data: BenchmarkMatrixResponse; error?: undefined }
  | { status: 'error'; data: BenchmarkMatrixResponse | null; error: string };

export type BenchmarkQualificationPreviewState =
  | { status: 'loading'; data: BenchmarkQualificationPreviewResponse | null; error?: undefined }
  | { status: 'ready'; data: BenchmarkQualificationPreviewResponse; error?: undefined }
  | { status: 'error'; data: BenchmarkQualificationPreviewResponse | null; error: string };

export type BenchmarkDriversState =
  | { status: 'loading'; data: BenchmarkSuiteDriverResponse[]; error?: undefined }
  | { status: 'ready'; data: BenchmarkSuiteDriverResponse[]; error?: undefined }
  | { status: 'error'; data: BenchmarkSuiteDriverResponse[]; error: string };

export type BenchmarkQualificationRowCheckState =
  | { status: 'loading'; data: BenchmarkQualificationResponse | null; error?: undefined }
  | { status: 'ready'; data: BenchmarkQualificationResponse; error?: undefined }
  | { status: 'error'; data: BenchmarkQualificationResponse | null; error: string };

export type BenchmarkQualificationRowChecks = Record<string, BenchmarkQualificationRowCheckState>;
