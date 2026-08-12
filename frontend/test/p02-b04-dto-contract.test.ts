import assert from 'node:assert/strict';
import test from 'node:test';

import { contentPageResponse } from '../src/dto-content';
import { dtoRecord } from '../src/dto-contract';
import { configResponse, instancesResponse } from '../src/dto-core';
import { installQueueStateResponse } from '../src/dto-install';
import { installItemFromQueuedViewModel, installQueueRequestFromItem } from '../src/install-item';
import { mockApi } from '../src/mock/api';
import { updateFlowFromResponse, updateInfoResponse } from '../src/updater';
import { createBackendViewResponse } from '../src/views/create/CreateView';
import { launcherAccountsResponse, savedSkinsResponse } from '../src/views/accounts/api';

test('P02-B04 retained bootstrap, update, content, and create DTOs reject malformed payloads', async () => {
  const config = dtoRecord(await mockApi('GET', '/config'), 'Config fixture');
  assert.equal(configResponse(config).username, 'MockPlayer');
  assert.throws(() => configResponse({ ...config, music_track: undefined }), /Config music track response was invalid/);

  const update = dtoRecord(await mockApi('GET', '/update'), 'Update fixture');
  assert.equal(typeof updateInfoResponse(update).latest_version, 'string');
  assert.throws(() => updateInfoResponse({ ...update, available: 'yes' }), /Update check response was invalid/);

  const flow = dtoRecord(await mockApi('GET', '/update/flow'), 'Update flow fixture');
  assert.equal(updateFlowFromResponse(flow).phase, 'idle');
  assert.throws(() => updateFlowFromResponse({ ...flow, phase: 'queued' }), /Update flow phase response was invalid/);

  const page = dtoRecord(await mockApi('GET', '/content/search?kind=mod'), 'Content fixture');
  assert.ok(contentPageResponse(page).items.length > 0);
  assert.throws(() => contentPageResponse({ ...page, total: 'many' }), /Content total response was invalid/);

  const create = dtoRecord(await mockApi('GET', '/instances/create-view'), 'Create fixture');
  const parsed = createBackendViewResponse(create);
  assert.ok(parsed.versions.every((version) => typeof version.create_enabled === 'boolean'));
  const firstVersion = dtoRecord(parsed.versions[0], 'Create version fixture');
  assert.throws(
    () => createBackendViewResponse({ ...create, versions: [{ ...firstVersion, create_enabled: undefined }] }),
    /Create version enabled response was invalid/,
  );
  assert.throws(
    () =>
      createBackendViewResponse({ ...create, sources: [{ id: 'unknown-loader', label: 'Unknown', enabled: true }] }),
    /Create source id response was invalid/,
  );

  const instanceList = dtoRecord(await mockApi('GET', '/instances'), 'Instances fixture');
  const parsedInstances = instancesResponse(instanceList);
  assert.ok(parsedInstances.instances.every((instance) => instance.version_display.minecraft_label.length > 0));
  const firstInstance = dtoRecord(parsedInstances.instances[0], 'Instance fixture');
  assert.throws(
    () => instancesResponse({ ...instanceList, instances: [{ ...firstInstance, version_display: undefined }] }),
    /Instance version display response was invalid/,
  );

  assert.equal(launcherAccountsResponse({ active_account_id: null, accounts: [null] }), null);
  assert.equal(savedSkinsResponse({ pending_apply_texture_key: null, skins: [null] }), null);
});

test('P02-B04 content queue DTO retains the exact strict retry request', () => {
  const response = installQueueStateResponse({
    active: null,
    items: [
      {
        queue_id: 'queue-1',
        state_id: 'queued',
        kind: 'content',
        title: 'Removing Sodium',
        label: 'Removing Sodium',
        summary: 'Queued',
        detail: 'Waiting',
        position: 1,
        total: 1,
        install_item: {
          version_id: 'instance-1',
          loader: null,
          content: {
            instance_id: 'instance-1',
            label: 'Removing Sodium',
            action: { kind: 'uninstall', canonical_ids: ['modrinth:sodium'] },
          },
        },
        remove_action: { action: 'remove', label: 'Remove', enabled: true, disabled_reason: null },
      },
    ],
    view_model: {
      state_id: 'queued',
      status_label: 'Queued',
      title: 'Install queue',
      summary: 'One queued item',
      queued_count: 1,
      queued_count_label: '1 queued',
      queued_item_label: 'Queued item',
      next_label: null,
      active_queued_count_label: null,
      section_title: 'Queue',
      empty_title: 'Nothing queued',
      empty_summary: 'The queue is empty',
    },
    notice: null,
    started_install: null,
    removed_instance_id: null,
  });
  const item = installItemFromQueuedViewModel(response.items[0]!);

  assert.deepEqual(installQueueRequestFromItem(item), {
    kind: 'content',
    instance_id: 'instance-1',
    label: 'Removing Sodium',
    action: { kind: 'uninstall', canonical_ids: ['modrinth:sodium'] },
  });
  const malformed = structuredClone(response);
  delete (malformed.items[0]!.install_item.content as { label?: string }).label;
  assert.throws(() => installQueueStateResponse(malformed), /Install content label response was invalid/);
});
