import type { LaunchProofView } from '../generated/LaunchProofView';
import type { BenchmarkMatrix } from '../generated/BenchmarkMatrix';
import type { QualificationResponse } from '../generated/QualificationResponse';
import type { BenchmarkDriverView } from '../generated/BenchmarkDriverView';

export type LaunchReportsState =
  | { status: 'loading'; data: LaunchProofView[]; error?: undefined }
  | { status: 'ready'; data: LaunchProofView[]; error?: undefined }
  | { status: 'error'; data: LaunchProofView[]; error: string };

export type BenchmarkMatrixState =
  | { status: 'loading'; data: BenchmarkMatrix | null; error?: undefined }
  | { status: 'ready'; data: BenchmarkMatrix; error?: undefined }
  | { status: 'error'; data: BenchmarkMatrix | null; error: string };

export type BenchmarkQualificationPreviewState =
  | { status: 'loading'; data: QualificationResponse | null; error?: undefined }
  | { status: 'ready'; data: QualificationResponse; error?: undefined }
  | { status: 'error'; data: QualificationResponse | null; error: string };

export type BenchmarkDriversState =
  | { status: 'loading'; data: BenchmarkDriverView[]; error?: undefined }
  | { status: 'ready'; data: BenchmarkDriverView[]; error?: undefined }
  | { status: 'error'; data: BenchmarkDriverView[]; error: string };

export type BenchmarkQualificationRowCheckState =
  | { status: 'loading'; data: QualificationResponse | null; error?: undefined }
  | { status: 'ready'; data: QualificationResponse; error?: undefined }
  | { status: 'error'; data: QualificationResponse | null; error: string };

export type BenchmarkQualificationRowChecks = Record<string, BenchmarkQualificationRowCheckState>;
