import assert from 'node:assert/strict';
import test from 'node:test';

import { ensureFlags, flagEnabled, refreshFlags, setFlagOverride } from '../src/flags';
import { mockApi } from '../src/mock/api';
import { featureFlags, featureFlagsLoadState } from '../src/store';
import type { FeatureFlagViewModel } from '../src/types-flags';

const inspectorKey = 'dev.state-inspector';

function inspectorFlag(): FeatureFlagViewModel {
  const flag = featureFlags.value?.find((candidate) => candidate.key === inspectorKey);
  assert.ok(flag, 'expected the inspector flag in the production flag store');
  return flag;
}

test('production flag actions preserve the local default, override, reset, and inspector gate', async (context) => {
  const originalFetch = globalThis.fetch;
  const requests: Array<{ body: unknown; method: string; path: string }> = [];
  let failNextGet = true;

  context.after(() => {
    globalThis.fetch = originalFetch;
    featureFlags.value = null;
    featureFlagsLoadState.value = { status: 'idle', error: null };
  });

  globalThis.fetch = (async (input, init) => {
    const method = init?.method ?? 'GET';
    const path = new URL(String(input), 'http://localhost').pathname.replace(/^\/api\/v1/, '');
    const body = typeof init?.body === 'string' ? JSON.parse(init.body) : undefined;
    requests.push({ body, method, path });

    if (method === 'GET' && path === '/flags' && failNextGet) {
      failNextGet = false;
      return Response.json({ error: 'temporary flag failure' }, { status: 503, statusText: 'Unavailable' });
    }

    const payload = await mockApi(method, path, body);
    return Response.json(payload);
  }) as typeof fetch;

  featureFlags.value = null;
  featureFlagsLoadState.value = { status: 'idle', error: null };

  await assert.rejects(refreshFlags(), /temporary flag failure/);
  assert.deepEqual(featureFlagsLoadState.value, { status: 'error', error: 'temporary flag failure' });

  await refreshFlags();
  assert.equal(featureFlagsLoadState.value.status, 'ready');
  assert.equal(inspectorFlag().source, 'default');
  assert.equal(inspectorFlag().enabled, false);
  assert.equal(flagEnabled(inspectorKey), false);

  const requestsBeforeEnsure = requests.length;
  await ensureFlags();
  assert.equal(requests.length, requestsBeforeEnsure);

  await setFlagOverride(inspectorKey, true);
  assert.equal(inspectorFlag().source, 'override');
  assert.equal(inspectorFlag().enabled, true);
  assert.equal(flagEnabled(inspectorKey), true);

  await setFlagOverride(inspectorKey, null);
  assert.equal(inspectorFlag().source, 'default');
  assert.equal(inspectorFlag().enabled, false);
  assert.equal(flagEnabled(inspectorKey), false);

  assert.deepEqual(requests, [
    { method: 'GET', path: '/flags', body: undefined },
    { method: 'GET', path: '/flags', body: undefined },
    { method: 'PUT', path: `/flags/${inspectorKey}`, body: { enabled: true } },
    { method: 'PUT', path: `/flags/${inspectorKey}`, body: { enabled: null } },
  ]);
});
