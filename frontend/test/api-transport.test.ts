import assert from 'node:assert/strict';
import test from 'node:test';
import { api, apiEventSourceUrl } from '../src/api';

const originalFetch = globalThis.fetch;

function requestFrom(input: string | URL | Request, init?: RequestInit): Request {
  return new Request(typeof input === 'string' ? new URL(input, 'http://localhost/') : input, init);
}

test.after(() => {
  globalThis.fetch = originalFetch;
});

test('JSON requests carry the process-local API capability', async () => {
  const observed: Request[] = [];
  globalThis.fetch = async (input, init) => {
    const request = requestFrom(input, init);
    observed.push(request);
    return new Response('{"ok":true}', {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    });
  };

  assert.deepEqual(await api('POST', '/status', { value: 1 }), { ok: true });
  assert.equal(observed.length, 1);
  assert.equal(observed[0]!.headers.get('X-Axial-Capability'), 'frontend-test-capability');
  assert.equal(observed[0]!.headers.get('Content-Type'), 'application/json');
});

test('SSE transport mints an exact path ticket without exposing the capability in its URL', async () => {
  const observed: Request[] = [];
  globalThis.fetch = async (input, init) => {
    observed.push(requestFrom(input, init));
    return new Response('{"ticket":"stream-ticket","expires_in_seconds":60}', {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    });
  };

  const url = await apiEventSourceUrl('/launch/session/events');
  assert.equal(observed.length, 1);
  assert.equal(observed[0]!.headers.get('X-Axial-Capability'), 'frontend-test-capability');
  assert.deepEqual(await observed[0]!.json(), {
    audience: 'stream',
    target: '/api/v1/launch/session/events',
  });
  assert.match(url, /[?&]axial_ticket=stream-ticket(?:&|$)/);
  assert.doesNotMatch(url, /frontend-test-capability/);
});
