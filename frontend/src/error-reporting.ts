import { api } from './api';

type FrontendErrorKind = 'error' | 'unhandledrejection' | 'render';

const MAX_REPORTS_PER_SESSION = 5;
const MAX_MESSAGE_CHARS = 200;

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
    reportBrowserError('error', error, message);
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

function reportBrowserError(kind: FrontendErrorKind, error: unknown, fallbackMessage?: unknown): void {
  try {
    if (reportingInFlight || consecutiveReportingFailures >= 3 || reportsSent >= MAX_REPORTS_PER_SESSION) return;

    const payload = errorPayload(kind, error, fallbackMessage);
    const dedupeKey = `${payload.kind}:${payload.name}:${payload.message}`;
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
        reportedKeys.delete(dedupeKey);
      })
      .finally(() => {
        reportingInFlight = false;
      });
  } catch {
    consecutiveReportingFailures += 1;
    if (reportingInFlight) {
      reportingInFlight = false;
    }
  }
}

function errorPayload(
  kind: FrontendErrorKind,
  error: unknown,
  fallbackMessage?: unknown,
): {
  kind: FrontendErrorKind;
  name: string;
  message: string;
} {
  return {
    kind,
    name: errorName(error),
    message: truncateMessage(errorMessage(error, fallbackMessage)),
  };
}

function errorName(error: unknown): string {
  if (error instanceof Error && typeof error.constructor?.name === 'string' && error.constructor.name.trim()) {
    return error.constructor.name;
  }
  return 'Error';
}

function errorMessage(error: unknown, fallbackMessage?: unknown): string {
  if (error instanceof Error && error.message) return safeString(error.message);
  if (fallbackMessage !== undefined) return safeString(fallbackMessage);
  return safeString(error);
}

function truncateMessage(message: string): string {
  const normalized = message.replace(/\s+/g, ' ').trim();
  if (normalized.length <= MAX_MESSAGE_CHARS) return normalized;
  return `${normalized.slice(0, MAX_MESSAGE_CHARS - 3)}...`;
}

function safeString(value: unknown): string {
  try {
    return String(value);
  } catch {
    return 'Unknown error';
  }
}
