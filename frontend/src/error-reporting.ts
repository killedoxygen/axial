import { api } from './api';
import { config } from './store';

type FrontendErrorKind = 'error' | 'unhandledrejection' | 'render';

const MAX_REPORTS_PER_SESSION = 5;

let initialized = false;
let reportsSent = 0;
let reportingInFlight = false;
let consecutiveReportingFailures = 0;
const reportedKeys = new Set<string>();

export function initErrorReporting(): void {
  if (initialized || typeof window === 'undefined') return;
  initialized = true;

  const previousOnError = window.onerror;
  window.onerror = (message, _source, _lineno, _colno, error): boolean | void => {
    reportBrowserError('error', error);
    if (typeof previousOnError === 'function') {
      return previousOnError(message, _source, _lineno, _colno, error);
    }
    return false;
  };

  window.addEventListener('unhandledrejection', (event) => {
    reportBrowserError('unhandledrejection', event.reason);
  });
}

export function reportRenderError(error: unknown): void {
  reportBrowserError('render', error);
}

function reportBrowserError(kind: FrontendErrorKind, error: unknown): void {
  let dedupeKey: string | undefined;
  try {
    if (
      config.peek()?.telemetry_enabled !== true ||
      reportingInFlight ||
      consecutiveReportingFailures >= 3 ||
      reportsSent >= MAX_REPORTS_PER_SESSION
    )
      return;

    const payload = errorPayload(kind, error);
    dedupeKey = `${payload.kind}:${payload.name}:${payload.message}`;
    if (reportedKeys.has(dedupeKey)) return;

    reportedKeys.add(dedupeKey);
    reportsSent += 1;
    reportingInFlight = true;

    void api('POST', '/telemetry/frontend-error', payload)
      .then(() => {
        consecutiveReportingFailures = 0;
      })
      .catch(() => {
        consecutiveReportingFailures += 1;
        if (dedupeKey !== undefined) reportedKeys.delete(dedupeKey);
      })
      .finally(() => {
        reportingInFlight = false;
      });
  } catch {
    consecutiveReportingFailures += 1;
    if (dedupeKey !== undefined) reportedKeys.delete(dedupeKey);
    if (reportingInFlight) {
      reportingInFlight = false;
    }
  }
}

function errorPayload(
  kind: FrontendErrorKind,
  error: unknown,
): {
  kind: FrontendErrorKind;
  name: string;
  message: string;
} {
  return {
    kind,
    name: errorName(error),
    // Error messages, stacks, filenames and thrown values can contain credentials,
    // paths or account names. Never read or send them, even to the local API.
    message:
      kind === 'render'
        ? 'Render failed.'
        : kind === 'unhandledrejection'
          ? 'Unhandled promise rejection.'
          : 'Browser error.',
  };
}

function errorName(error: unknown): string {
  if (error instanceof TypeError) return 'TypeError';
  if (error instanceof ReferenceError) return 'ReferenceError';
  if (error instanceof RangeError) return 'RangeError';
  if (error instanceof SyntaxError) return 'SyntaxError';
  if (error instanceof URIError) return 'URIError';
  if (error instanceof EvalError) return 'EvalError';
  return 'Error';
}
