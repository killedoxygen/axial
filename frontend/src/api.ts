import { getNativeApiBaseUrl } from './native';

declare const __CROOPOR_WEB_API_BASE__: string;

const API_PATH = '/api/v1';

let apiBaseUrl = '';

export let API = API_PATH;

export async function initializeApiBase(): Promise<void> {
  let nativeBaseUrl: string | null | undefined;
  try {
    nativeBaseUrl = await getNativeApiBaseUrl();
  } catch {
    nativeBaseUrl = undefined;
  }
  setApiBaseUrl(nativeBaseUrl ?? __CROOPOR_WEB_API_BASE__ ?? '');
}

export function setApiBaseUrl(baseUrl: string): void {
  apiBaseUrl = normalizeApiBaseUrl(baseUrl);
  API = `${apiBaseUrl}${API_PATH}`;
}

export function apiUrl(path: string): string {
  return `${API}${path.startsWith('/') ? path : `/${path}`}`;
}

export function apiResourceUrl(path: string): string {
  const trimmed = path.trim();
  if (/^[a-z][a-z\d+\-.]*:/i.test(trimmed) || trimmed.startsWith('//')) return trimmed;
  if (trimmed === API_PATH) return API;
  if (trimmed.startsWith(`${API_PATH}/`)) return apiUrl(trimmed.slice(API_PATH.length));
  return apiUrl(trimmed);
}

export interface ApiError extends Error {
  name: 'ApiError';
  status: number;
  statusText: string;
  payload?: unknown;
}

export function isApiError(error: unknown): error is ApiError {
  return error instanceof Error
    && error.name === 'ApiError'
    && typeof (error as Partial<ApiError>).status === 'number';
}

export async function api(method: string, path: string, body?: unknown): Promise<any> {
  const opts: RequestInit = { method };
  if (body !== undefined) {
    opts.headers = { 'Content-Type': 'application/json' };
    opts.body = JSON.stringify(body);
  }
  const response = await fetch(apiUrl(path), opts);
  const payload = await readJsonPayload(response);
  if (!response.ok) {
    throw makeApiError(response, payload);
  }
  return payload;
}

function normalizeApiBaseUrl(baseUrl: string): string {
  const trimmed = baseUrl.trim().replace(/\/+$/, '');
  if (trimmed.endsWith(API_PATH)) return trimmed.slice(0, -API_PATH.length);
  return trimmed;
}

async function readJsonPayload(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text.trim()) return undefined;
  if (!response.ok && !looksJson(response, text)) return undefined;
  try {
    return JSON.parse(text);
  } catch (error) {
    if (response.ok) throw error;
    return undefined;
  }
}

function looksJson(response: Response, text: string): boolean {
  const contentType = response.headers.get('content-type') || '';
  if (contentType.toLowerCase().includes('json')) return true;
  return /^[\[{]/.test(text.trim());
}

function makeApiError(response: Response, payload: unknown): ApiError {
  const error = new Error(apiErrorMessage(response, payload)) as ApiError;
  error.name = 'ApiError';
  error.status = response.status;
  error.statusText = response.statusText;
  if (payload !== undefined) error.payload = payload;
  return error;
}

function apiErrorMessage(response: Response, payload: unknown): string {
  if (isErrorPayload(payload)) return boundedErrorMessage(payload.error);
  const statusText = response.statusText.trim();
  return boundedErrorMessage(`Request failed with HTTP ${response.status}${statusText ? ` ${statusText}` : ''}`);
}

function isErrorPayload(payload: unknown): payload is { error: string } {
  return typeof payload === 'object'
    && payload !== null
    && typeof (payload as { error?: unknown }).error === 'string'
    && (payload as { error: string }).error.trim().length > 0;
}

function boundedErrorMessage(value: string): string {
  const normalized = value.trim().replace(/\s+/g, ' ');
  return normalized.length > 180 ? `${normalized.slice(0, 177)}...` : normalized;
}
