import { getNativeApiTransportBootstrap } from './native';

declare const __AXIAL_WEB_API_BASE__: string;
declare const __AXIAL_TEST_API_CAPABILITY__: string;

const API_PATH = '/api/v1';
const WEB_API_BASE = normalizeApiBaseUrl(__AXIAL_WEB_API_BASE__ ?? '');

let apiBaseUrl = WEB_API_BASE;
let apiCapability = __AXIAL_TEST_API_CAPABILITY__.trim();
let mediaTicket = '';
let mediaTicketRefreshAt = 0;
let mediaTicketRefreshTimer: number | undefined;
let nativeTransport = false;
let transportRecoveryPromise: Promise<void> | null = null;
let apiBaseInitialized = false;
let apiBaseInitPromise: Promise<void> | null = null;

export let API = `${apiBaseUrl}${API_PATH}`;

export async function initializeApiBase(): Promise<void> {
  if (apiBaseInitialized) return;
  if (apiBaseInitPromise) return apiBaseInitPromise;

  apiBaseInitPromise = resolveApiBase();
  try {
    await apiBaseInitPromise;
  } catch (error) {
    apiBaseInitPromise = null;
    throw error;
  }
}

async function resolveApiBase(): Promise<void> {
  const native = await getNativeApiTransportBootstrap();
  nativeTransport = native !== null;
  if (native) {
    setApiBaseUrl(native.base_url);
    apiCapability = native.capability.trim();
  } else {
    setApiBaseUrl(__AXIAL_WEB_API_BASE__ ?? '');
    apiCapability = __AXIAL_TEST_API_CAPABILITY__.trim();
    if (!apiCapability && !__AXIAL_MOCK_API__) {
      const response = await fetch(apiUrl('/transport/bootstrap'), { method: 'POST' });
      const payload = await readJsonPayload(response);
      if (!response.ok) throw makeApiError(response, payload);
      if (
        typeof payload !== 'object' ||
        payload === null ||
        typeof (payload as Partial<{ base_url: string }>).base_url !== 'string' ||
        typeof (payload as Partial<{ capability: string }>).capability !== 'string'
      ) {
        throw new Error('API transport bootstrap response was invalid.');
      }
      setApiBaseUrl((payload as { base_url: string }).base_url);
      apiCapability = (payload as { capability: string }).capability.trim();
    }
  }
  if (!__AXIAL_MOCK_API__) {
    requireApiCapability();
    if (typeof window !== 'undefined') await refreshMediaTicket();
  }
  apiBaseInitialized = true;
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
  if (isAbsoluteLikeUrl(trimmed)) {
    const apiPath = apiOwnedResourcePath(trimmed);
    if (apiPath !== null) return withMediaTicket(apiPath ? apiUrl(apiPath) : API);
    return withMediaTicket(apiUrl(trimmed));
  }
  if (trimmed === API_PATH) return withMediaTicket(API);
  if (trimmed.startsWith(`${API_PATH}/`)) return withMediaTicket(apiUrl(trimmed.slice(API_PATH.length)));
  return withMediaTicket(apiUrl(trimmed));
}

export async function apiEventSourceUrl(path: string): Promise<string> {
  await initializeApiBase();
  const target = apiPath(path);
  const grant = await mintTicket('stream', target);
  return appendTicket(`${apiBaseUrl}${target}`, grant.ticket);
}

export interface ApiError extends Error {
  name: 'ApiError';
  status: number;
  statusText: string;
  payload?: unknown;
}

export function isApiError(error: unknown): error is ApiError {
  return error instanceof Error && error.name === 'ApiError' && typeof (error as Partial<ApiError>).status === 'number';
}

export async function api<T = any>(method: string, path: string, body?: unknown): Promise<T> {
  if (__AXIAL_MOCK_API__) {
    const { mockApi } = await import('./mock/api');
    return mockApi<T>(method, path, body);
  }
  const opts: RequestInit = { method };
  if (body !== undefined) {
    opts.headers = { 'Content-Type': 'application/json' };
    opts.body = JSON.stringify(body);
  }
  const response = await apiFetch(apiUrl(path), opts);
  const payload = await readJsonPayload(response);
  if (!response.ok) {
    throw makeApiError(response, payload);
  }
  return payload as T;
}

export async function apiFetch(input: string, init: RequestInit = {}): Promise<Response> {
  await initializeApiBase();
  if (__AXIAL_MOCK_API__) return fetch(input, init);
  if (!__AXIAL_MOCK_API__ && typeof window !== 'undefined' && Date.now() >= mediaTicketRefreshAt) {
    await refreshMediaTicket();
  }
  const target = new URL(input, browserBaseUrl());
  const apiOrigin = new URL(API, browserBaseUrl());
  if (target.origin !== apiOrigin.origin || !target.pathname.startsWith(`${API_PATH}/`)) {
    throw new Error('Authenticated API fetch target is outside the local API.');
  }
  const headers = new Headers(init.headers);
  headers.set('X-Axial-Capability', requireApiCapability());
  let response = await fetch(input, { ...init, headers });
  if (response.status === 401 && (await recoverBrowserTransport())) {
    headers.set('X-Axial-Capability', requireApiCapability());
    response = await fetch(input, { ...init, headers });
  }
  return response;
}

interface TicketGrant {
  ticket: string;
  expires_in_seconds: number;
}

async function refreshMediaTicket(): Promise<void> {
  const grant = await mintTicket('media');
  mediaTicket = grant.ticket;
  const refreshDelay = Math.max(1, grant.expires_in_seconds - 60) * 1000;
  mediaTicketRefreshAt = Date.now() + refreshDelay;
  if (typeof window !== 'undefined') {
    if (mediaTicketRefreshTimer !== undefined) window.clearTimeout(mediaTicketRefreshTimer);
    mediaTicketRefreshTimer = window.setTimeout(() => {
      void refreshMediaTicket().catch(() => {
        mediaTicket = '';
        mediaTicketRefreshAt = 0;
      });
    }, refreshDelay);
  }
}

async function mintTicket(audience: 'media' | 'stream', target?: string): Promise<TicketGrant> {
  let response = await fetch(apiUrl('/transport/tickets'), {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'X-Axial-Capability': requireApiCapability(),
    },
    body: JSON.stringify(target === undefined ? { audience } : { audience, target }),
  });
  if (response.status === 401 && (await recoverBrowserTransport())) {
    response = await fetch(apiUrl('/transport/tickets'), {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'X-Axial-Capability': requireApiCapability(),
      },
      body: JSON.stringify(target === undefined ? { audience } : { audience, target }),
    });
  }
  const payload = await readJsonPayload(response);
  if (!response.ok) throw makeApiError(response, payload);
  if (
    typeof payload !== 'object' ||
    payload === null ||
    typeof (payload as Partial<TicketGrant>).ticket !== 'string' ||
    typeof (payload as Partial<TicketGrant>).expires_in_seconds !== 'number'
  ) {
    throw new Error('API transport ticket response was invalid.');
  }
  return payload as TicketGrant;
}

async function recoverBrowserTransport(): Promise<boolean> {
  if (transportRecoveryPromise) {
    await transportRecoveryPromise;
    return true;
  }
  if (nativeTransport || __AXIAL_TEST_API_CAPABILITY__ || !apiBaseInitialized) return false;
  transportRecoveryPromise ??= (async () => {
    apiBaseInitialized = false;
    apiBaseInitPromise = null;
    apiCapability = '';
    mediaTicket = '';
    mediaTicketRefreshAt = 0;
    if (mediaTicketRefreshTimer !== undefined && typeof window !== 'undefined') {
      window.clearTimeout(mediaTicketRefreshTimer);
      mediaTicketRefreshTimer = undefined;
    }
    await initializeApiBase();
  })();
  try {
    await transportRecoveryPromise;
  } finally {
    transportRecoveryPromise = null;
  }
  return true;
}

function requireApiCapability(): string {
  if (!apiCapability) throw new Error('Local API capability is unavailable.');
  return apiCapability;
}

function withMediaTicket(url: string): string {
  if (__AXIAL_MOCK_API__) return url;
  if (!mediaTicket) return 'about:blank';
  return appendTicket(url, mediaTicket);
}

function appendTicket(value: string, ticket: string): string {
  const url = new URL(value, browserBaseUrl());
  url.searchParams.set('axial_ticket', ticket);
  return url.href;
}

function apiPath(path: string): string {
  const normalized = path.startsWith('/') ? path : `/${path}`;
  return normalized.startsWith(API_PATH) ? normalized : `${API_PATH}${normalized}`;
}

function normalizeApiBaseUrl(baseUrl: string): string {
  const trimmed = baseUrl.trim().replace(/\/+$/, '');
  if (trimmed.endsWith(API_PATH)) return trimmed.slice(0, -API_PATH.length);
  return trimmed;
}

function isAbsoluteLikeUrl(value: string): boolean {
  return /^[a-z][a-z\d+\-.]*:/i.test(value) || value.startsWith('//');
}

function apiOwnedResourcePath(value: string): string | null {
  const currentApiUrl = parseUrl(API);
  const resourceUrl = parseUrl(value);
  if (!currentApiUrl || !resourceUrl) return null;
  if (resourceUrl.protocol !== currentApiUrl.protocol || resourceUrl.host !== currentApiUrl.host) return null;

  const apiPath = currentApiUrl.pathname.replace(/\/+$/, '');
  if (resourceUrl.pathname === apiPath) return '';
  if (!resourceUrl.pathname.startsWith(`${apiPath}/`)) return null;
  return `${resourceUrl.pathname.slice(apiPath.length)}${resourceUrl.search}${resourceUrl.hash}`;
}

function parseUrl(value: string): URL | null {
  try {
    return new URL(value, browserBaseUrl());
  } catch {
    return null;
  }
}

function browserBaseUrl(): string {
  if (typeof location !== 'undefined' && location.href) return location.href;
  return 'http://localhost/';
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
  return (
    typeof payload === 'object' &&
    payload !== null &&
    typeof (payload as { error?: unknown }).error === 'string' &&
    (payload as { error: string }).error.trim().length > 0
  );
}

function boundedErrorMessage(value: string): string {
  const normalized = value.trim().replace(/\s+/g, ' ');
  return normalized.length > 180 ? `${normalized.slice(0, 177)}...` : normalized;
}
