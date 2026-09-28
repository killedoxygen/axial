import type { ApplicationViewModelTone } from './types-performance';

interface PerformanceHealthNotice {
  tone: 'warned' | 'error';
  title: string;
  detail: string;
}

interface PerformanceHealthNoticeSource {
  view_model?: {
    tone: ApplicationViewModelTone;
    title?: string;
    detail?: string;
  } | null;
}

export function performanceHealthNotice(source: PerformanceHealthNoticeSource | null): PerformanceHealthNotice | null {
  const viewModel = source?.view_model;
  if (
    !viewModel ||
    (viewModel.tone !== 'warn' && viewModel.tone !== 'err') ||
    typeof viewModel.title !== 'string' ||
    typeof viewModel.detail !== 'string'
  ) {
    return null;
  }
  return {
    tone: viewModel.tone === 'warn' ? 'warned' : 'error',
    title: viewModel.title,
    detail: viewModel.detail,
  };
}
