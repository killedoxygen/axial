export const API = '/api/v1';

export async function api(method: string, path: string, body?: unknown): Promise<any> {
  const opts: RequestInit = { method, headers: { 'Content-Type': 'application/json' } };
  if (body) opts.body = JSON.stringify(body);
  return (await fetch(`${API}${path}`, opts)).json();
}
