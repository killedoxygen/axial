import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import test from 'node:test';

import { dtoArray, dtoRecord, dtoString } from '../../src/dto-contract';
import { enrichedInstanceResponse, instancesResponse } from '../../src/dto-core';
import { mockApi } from '../../src/mock/api';

test('mock Custom Java survives cold detail and list reads without exposing private settings', async () => {
  const view = dtoRecord(await mockApi('GET', '/instances/create-view?source=vanilla'), 'Create view');
  const choice = dtoArray(view.versions, 'Create versions')
    .map((row) => dtoRecord(row, 'Create version'))
    .find((row) => row.source_id === 'vanilla' && row.create_enabled === true);
  assert.ok(choice, 'the public create view offers a disposable Vanilla instance');
  const created = enrichedInstanceResponse(
    await mockApi('POST', '/instances', {
      name: `Java selection ${randomUUID()}`,
      selection_id: dtoString(choice.selection_id, 'Create selection'),
    }),
  );
  assert.deepEqual(created.java_selection, { kind: 'inherited' });

  const path = `/instances/${encodeURIComponent(created.id)}`;
  const saved = dtoRecord(
    await mockApi('PUT', path, {
      expected_revision: created.revision,
      java_path: '/fixture/custom-java/bin/java',
      extra_jvm_args: '-Dfixture.private=value',
    }),
    'Saved instance',
  );
  const cold = dtoRecord(await mockApi('GET', path), 'Cold instance');
  const list = dtoRecord(await mockApi('GET', '/instances'), 'Instance list');
  const listed = instancesResponse(list).instances.find((instance) => instance.id === created.id);
  const listedWire = dtoArray(list.instances, 'Instance rows')
    .map((row) => dtoRecord(row, 'Instance row'))
    .find((row) => row.id === created.id);
  assert.ok(listed);
  assert.ok(listedWire);

  for (const instance of [enrichedInstanceResponse(cold), enrichedInstanceResponse(saved), listed]) {
    assert.equal(instance.id, created.id);
    assert.equal(instance.revision, created.revision + 1);
    assert.deepEqual(instance.java_selection, { kind: 'custom' });
  }
  for (const wire of [saved, cold, listedWire]) {
    assert.equal(wire.java_path ?? '', '');
    assert.equal(wire.extra_jvm_args ?? '', '');
  }
});

test('mock component Java survives cold reads and stale saves until explicitly cleared', async () => {
  const view = dtoRecord(await mockApi('GET', '/instances/create-view?source=vanilla'), 'Create view');
  const choice = dtoArray(view.versions, 'Create versions')
    .map((row) => dtoRecord(row, 'Create version'))
    .find((row) => row.source_id === 'vanilla' && row.create_enabled === true);
  assert.ok(choice);
  const created = enrichedInstanceResponse(
    await mockApi('POST', '/instances', {
      name: `Java component ${randomUUID()}`,
      selection_id: dtoString(choice.selection_id, 'Create selection'),
    }),
  );
  const path = `/instances/${encodeURIComponent(created.id)}`;
  const saved = enrichedInstanceResponse(
    await mockApi('PUT', path, {
      expected_revision: created.revision,
      java_path: 'jre-legacy',
    }),
  );
  const cold = enrichedInstanceResponse(await mockApi('GET', path));
  const listed = instancesResponse(await mockApi('GET', '/instances')).instances.find(
    (instance) => instance.id === created.id,
  );
  assert.ok(listed);
  for (const instance of [cold, saved, listed]) {
    assert.equal(instance.id, created.id);
    assert.equal(instance.revision, created.revision + 1);
    assert.deepEqual(instance.java_selection, { kind: 'component', component: 'jre-legacy' });
    assert.equal(instance.java_path ?? '', '');
    assert.equal(instance.extra_jvm_args ?? '', '');
  }

  await assert.rejects(
    mockApi('PUT', path, {
      expected_revision: created.revision,
      java_path: '',
    }),
    { name: 'ApiError', status: 409 },
  );
  assert.deepEqual(enrichedInstanceResponse(await mockApi('GET', path)), cold);
  assert.deepEqual(
    instancesResponse(await mockApi('GET', '/instances')).instances.find((instance) => instance.id === created.id),
    listed,
  );

  const cleared = enrichedInstanceResponse(
    await mockApi('PUT', path, {
      expected_revision: saved.revision,
      java_path: ' \t\n ',
    }),
  );
  const clearedCold = enrichedInstanceResponse(await mockApi('GET', path));
  const clearedList = instancesResponse(await mockApi('GET', '/instances')).instances.find(
    (instance) => instance.id === created.id,
  );
  assert.ok(clearedList);
  for (const instance of [cleared, clearedCold, clearedList]) {
    assert.equal(instance.id, created.id);
    assert.equal(instance.revision, created.revision + 2);
    assert.deepEqual(instance.java_selection, { kind: 'inherited' });
    assert.equal(instance.java_path ?? '', '');
    assert.equal(instance.extra_jvm_args ?? '', '');
  }
});
