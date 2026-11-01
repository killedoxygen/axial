import assert from 'node:assert/strict';
import test from 'node:test';
import { api, apiFetch, initializeApiBase, isApiError, setApiBaseUrl } from '../src/api';
import { configResponse } from '../src/dto-core';

test('config decoder validates revisions and retained fields and projects away internal data', () => {
  const fixture = {
    revision: 4,
    account_selection_revision: 7,
    username: 'A',
    launch_auth_mode: 'online',
    max_memory_mb: 4096,
    min_memory_mb: 512,
    java_path_override: '',
    window_width: 0,
    window_height: 0,
    jvm_preset: '',
    performance_mode: 'managed',
    theme: '',
    custom_hue: null,
    custom_vibrancy: null,
    lightness: null,
    onboarding_done: false,
    telemetry_enabled: false,
    discord_rpc_enabled: true,
    discord_rpc_onboarding_seen: false,
    music_enabled: null,
    music_volume: null,
    music_track: 0,
  };
  assert.deepEqual(configResponse(fixture), fixture);
  for (const field of ['revision', 'account_selection_revision']) {
    for (const invalid of [undefined, null, '4', -1, 0.5, Number.MAX_SAFE_INTEGER + 1, Infinity]) {
      assert.throws(() => configResponse({ ...fixture, [field]: invalid }), /Config/);
    }
  }
  for (const invalid of [
    { launch_auth_mode: 'guest' },
    { theme: 'guardian' },
    { performance_mode: 'unknown' },
    { jvm_preset: 'unsupported' },
    { telemetry_enabled: 'true' },
    { music_enabled: 'true' },
    { music_track: undefined },
    { max_memory_mb: NaN },
    { custom_hue: '12' },
  ])
    assert.throws(() => configResponse({ ...fixture, ...invalid }), /Config/);
  const projected = configResponse({
    ...fixture,
    telemetry_install_id: 'private',
    library_dir: '/private',
    guardian_mode: 'managed',
  });
  assert.deepEqual(projected, fixture);
  assert.equal(configResponse({ ...fixture, custom_hue: 12 }).custom_hue, 12);
  assert.equal(configResponse({ ...fixture, music_enabled: false }).music_enabled, false);
});

test('client rejects HTTP errors without exposing raw rejection bodies and bounds public messages', async () => {
  const originalFetch = globalThis.fetch;
  try {
    for (const body of ['private SQL at /private/profile', '<html>private proxy detail</html>', '{bad-private-json']) {
      globalThis.fetch = async () => new Response(body, { status: 400, headers: { 'content-type': 'text/plain' } });
      await assert.rejects(api('POST', '/config', {}), (error) => {
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
    await assert.rejects(api('PUT', '/config', {}), (error) => {
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
