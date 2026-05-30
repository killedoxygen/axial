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

export async function api(method: string, path: string, body?: unknown): Promise<any> {
  const opts: RequestInit = { method };
  if (body !== undefined) {
    opts.headers = { 'Content-Type': 'application/json' };
    opts.body = JSON.stringify(body);
  }
  return (await fetch(apiUrl(path), opts)).json();
}

function normalizeApiBaseUrl(baseUrl: string): string {
  const trimmed = baseUrl.trim().replace(/\/+$/, '');
  if (trimmed.endsWith(API_PATH)) return trimmed.slice(0, -API_PATH.length);
  return trimmed;
}
