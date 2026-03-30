import { API } from './state.js';

export { API };

export async function api(method, path, body) {
  const opts = { method, headers: { 'Content-Type': 'application/json' } };
  if (body) opts.body = JSON.stringify(body);
  return (await fetch(`${API}${path}`, opts)).json();
}
