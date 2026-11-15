import type { ToastKind } from '../app/toast';
import type { CreateResultView } from '../generated/CreateResultView';
import type { Tone } from '../generated/Tone';

export function createToastKind(tone: Tone): ToastKind {
  if (tone === 'err') return 'error';
  if (tone === 'warn') return 'info';
  return 'success';
}

/** The result's summary and detail as one sentence, without repeating either. */
export function createResultToastMessage(view: CreateResultView): string {
  const parts: string[] = [];
  for (const value of [view.summary.trim(), view.detail?.trim() ?? '']) {
    if (value && !parts.some((part) => part.includes(value))) parts.push(value);
  }
  return parts.join(' ');
}
