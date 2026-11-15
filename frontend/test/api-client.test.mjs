import assert from 'node:assert/strict';
import test from 'node:test';
import { apiFetch, initializeApiBase, isApiError, request, setApiBaseUrl } from '../src/platform/transport';

test('client rejects HTTP errors without exposing raw rejection bodies and bounds public messages', async () => {
  const originalFetch = globalThis.fetch;
  try {
    for (const body of ['private SQL at /private/profile', '<html>private proxy detail</html>', '{bad-private-json']) {
      globalThis.fetch = async () => new Response(body, { status: 400, headers: { 'content-type': 'text/plain' } });
      await assert.rejects(request('POST', '/config', {}), (error) => {
        assert.ok(isApiError(error));
        assert.equal(error.status, 400);
        assert.equal(error.payload, undefined);
        assert.equal(error.message, 'Request failed with HTTP 400');
        return true;
      });
    }
    globalThis.fetch = async () =>
      new Response(JSON.stringify({ error: `  ${'bounded '.repeat(100)}  ` }), {
        status: 422,
        headers: { 'content-type': 'application/json' },
      });
    await assert.rejects(request('PUT', '/config', {}), (error) => {
      assert.ok(isApiError(error));
      assert.equal(error.status, 422);
      assert.equal(error.message.length, 180);
      assert.ok(error.message.endsWith('...'));
      return true;
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('authenticated fetch refuses off-origin targets and never retries a rejected domain mutation', async () => {
  const originalFetch = globalThis.fetch;
  let requests = 0;
  try {
    await initializeApiBase();
    setApiBaseUrl('http://127.0.0.1:38471');
    globalThis.fetch = async (_input, init) => {
      requests += 1;
      assert.equal(new Headers(init?.headers).get('X-Axial-Capability'), 'frontend-test-capability');
      assert.equal(init?.redirect, 'error');
      return new Response('', { status: 401 });
    };
    for (const target of [
      'https://example.com/api/v1/config',
      'http://127.0.0.1:38472/api/v1/config',
      'http://127.0.0.1:38471/elsewhere',
      'http://127.0.0.1:38471/api/v10/config',
    ])
      await assert.rejects(apiFetch(target), /outside the local API/);
    assert.equal(requests, 0);
    const response = await apiFetch('http://127.0.0.1:38471/api/v1/config', { method: 'PUT', body: '{}' });
    assert.equal(response.status, 401);
    assert.equal(requests, 1);
    for (const base of [
      'https://127.0.0.1:38471',
      'http://example.com',
      'http://user@127.0.0.1:38471',
      'http://127.0.0.1:38471/private',
    ]) {
      assert.throws(() => setApiBaseUrl(base), /loopback HTTP origin/);
    }
  } finally {
    globalThis.fetch = originalFetch;
    setApiBaseUrl('');
  }
});
