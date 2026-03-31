export const API = '/api/v1';

export async function api(method: string, path: string, body?: unknown): Promise<any> {
  const opts: RequestInit = { method };
  if (body !== undefined) {
    opts.headers = { 'Content-Type': 'application/json' };
    opts.body = JSON.stringify(body);
  }
  return (await fetch(`${API}${path}`, opts)).json();
}
