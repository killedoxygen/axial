import { getNativeApiBaseUrl } from './native';

declare const __CROOPOR_WEB_API_BASE__: string;

const API_PATH = '/api/v1';

let apiBaseUrl = '';

export let API = API_PATH;

export async function initializeApiBase(): Promise<void> {
  const nativeBaseUrl = await getNativeApiBaseUrl();
  setApiBaseUrl(nativeBaseUrl ?? __CROOPOR_WEB_API_BASE__ ?? '');
}

export function setApiBaseUrl(baseUrl: string): void {
  apiBaseUrl = normalizeApiBaseUrl(baseUrl);
  API = `${apiBaseUrl}${API_PATH}`;
}

export function apiUrl(path: string): string {
  return `${API}${path.startsWith('/') ? path : `/${path}`}`;
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
