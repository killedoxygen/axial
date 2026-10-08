import assert from 'node:assert/strict';
import test from 'node:test';

import { instancesResponse } from '../../src/dto-core';
import { launchSessionsResponse } from '../../src/launch-response-adapters';
import { mockApi } from '../../src/mock/api';

test('mock startup hydrates idle sessions without pretending Launch is supported', async () => {
  const snapshot = await mockApi('GET', '/launch/sessions');
  assert.deepEqual(snapshot, { sessions: [] });
  assert.deepEqual(launchSessionsResponse(snapshot), {});

  const instance = instancesResponse(await mockApi('GET', '/instances')).instances[0];
  assert.ok(instance);
  await assert.rejects(mockApi('POST', '/launch', { instance_id: instance.id }), { name: 'ApiError', status: 501 });
  const afterRefusal = await mockApi('GET', '/launch/sessions');
  assert.deepEqual(afterRefusal, { sessions: [] });
  assert.deepEqual(launchSessionsResponse(afterRefusal), {});
});
