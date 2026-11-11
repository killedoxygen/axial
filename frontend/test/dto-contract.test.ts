import assert from 'node:assert/strict';
import test from 'node:test';

import {
  contentCompatResponse,
  contentPageResponse,
  instanceContentResponse,
  resolutionPlanResponse,
} from '../src/dto-content';
import { dtoRecord } from '../src/dto-contract';
import { configResponse, instancesResponse } from '../src/dto-core';
import { installQueueStateResponse } from '../src/dto-install';
import { installItemFromQueuedViewModel, installQueueRequestFromItem } from '../src/install-item';
import { mockApi } from '../src/mock/api';
import { updateFlowFromResponse, updateInfoResponse } from '../src/updater';
import { createBackendViewResponse } from '../src/views/create/CreateView';

test('content plans accept omitted empty dependencies from the live Rust response', () => {
  // Captured from /content/plan for Sodium on Fabric 1.20.1.
  const item = {
    canonical_id: 'modrinth:AANobbMI',
    title: 'Sodium',
    kind: 'mod',
    project_id: 'AANobbMI',
    version_id: 'OihdIimA',
    version_number: 'mc1.20.1-0.5.13-fabric',
    filename: 'sodium-fabric-0.5.13+mc1.20.1.jar',
    sha1: 'bcdbf37d9494e405cba210d7a80ed58e756297b5',
    size: 971552,
    reason: 'selected',
    already_installed: false,
    update: false,
  };
  const plan = {
    instance_id: '6bf331ba-fb13-44e0-8e46-93ffe80cd285',
    loader: 'fabric',
    game_version: '1.20.1',
    items: [item],
    conflicts: [],
    total_download_bytes: 971552,
  };
  assert.deepEqual(resolutionPlanResponse(plan).items[0]?.dependencies, []);
  const parse = (dependencies: unknown) => resolutionPlanResponse({ ...plan, items: [{ ...item, dependencies }] });
  assert.deepEqual(parse([]).items[0]?.dependencies, []);
  const dependencies = [{ project_id: 'fabric-api', kind: 'required' }];
  assert.equal(parse(dependencies).items[0]?.dependencies[0]?.project_id, 'fabric-api');
  for (const invalid of [null, {}, 'none', [null], [{ kind: 'unknown' }]]) {
    assert.throws(() => parse(invalid), /Content (plan dependencies|dependency)/);
  }
});

test('instance content keeps a local modpack provider from the live Rust response', () => {
  // Captured from /instances/{id}/content after importing an unpublished .mrpack.
  const hash =
    '3bc14fa86e4be2e65df007db609cfba4575d9757ec2aa1636af567c9ea44c58eec623a23a170b4e953917c1849c64ccafd2390e8b88985e7c36f30667f5824e0';
  const response = instanceContentResponse({
    entries: [
      {
        canonical_id: 'modrinth:yfDziwn1',
        enabled: true,
        filename: 'SodiumTranslations.zip',
        kind: 'resource_pack',
        project_id: 'yfDziwn1',
        provider: 'modrinth',
        title: 'Translations for Sodium',
        version_id: 'bo4O33l6',
      },
    ],
    packs: [
      {
        canonical_id: `local:${hash}`,
        paths: ['resourcepacks/SodiumTranslations.zip', 'config/axial-smoke.txt', 'options.txt'],
        project_id: hash,
        provider: 'local',
        title: 'Private Smoke Pack',
        version_id: hash,
        version_number: '1.0.0',
      },
    ],
  });
  assert.equal(response.entries[0]?.provider, 'modrinth');
  assert.equal(response.packs[0]?.provider, 'local');
  assert.equal(response.packs[0]?.title, 'Private Smoke Pack');
  assert.throws(() => instanceContentResponse({ entries: [], packs: [{ ...response.packs[0], provider: 'other' }] }));
});

test('content compatibility maps the Rust empty Vanilla key without accepting malformed loaders', () => {
  const candidate = {
    loader: '',
    loader_label: 'Vanilla',
    game_version: '1.20.1',
    selection_id: 'vanilla|1.20.1',
    summary: 'Works here',
    supported_count: 1,
    total_count: 1,
    complete: true,
    drops: [],
  };
  const parse = (loader: unknown) => contentCompatResponse({ candidates: [{ ...candidate, loader }] }).candidates[0]!;
  assert.equal(parse('').loader, 'vanilla');
  assert.equal(parse('').selection_id, 'vanilla|1.20.1');
  for (const loader of ['vanilla', 'fabric', 'quilt', 'forge', 'neoforge']) {
    assert.equal(parse(loader).loader, loader);
  }
  for (const invalid of [undefined, null, 0, 'unknown']) {
    assert.throws(() => parse(invalid), /Compatibility loader/);
  }
});

test('dto-validation retained bootstrap, update, content, and create DTOs reject malformed payloads', async () => {
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
});

test('dto-validation content queue DTO retains the exact strict retry request', () => {
  const response = installQueueStateResponse({
    queue_epoch: 'queue-process-1',
    revision: 4,
    registry_revision: 1,
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
        remove_action: { action: 'remove_from_queue', label: 'Remove', enabled: true, disabled_reason: null },
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
  assert.equal(response.queue_epoch, 'queue-process-1');
  assert.equal(response.registry_revision, 1);
  assert.throws(() => installQueueStateResponse({ ...response, registry_revision: -1 }), /safe unsigned integer/);
  assert.throws(() => installQueueStateResponse({ ...response, queue_epoch: '' }), /epoch/);

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
