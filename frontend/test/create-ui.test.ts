import assert from 'node:assert/strict';
import test from 'node:test';
import { apiModule, loadSource } from './source';
import { enrichedInstance, queueSnapshot } from './wire';
import * as presenters from '../src/create/presenters';
import {
  CHANNEL_LABEL,
  CHANNEL_ORDER,
  createLoaderSelection,
  normalizeChannel,
  versionRow,
  type CreateSourceId,
  type VersionRowModel,
} from '../src/create/versions';
import { parseVersionQuery, rankVersions } from '../src/create/search';
import type { Channel, LoaderKey } from '../src/create/defaults';
import type { CreateInstanceResponse } from '../src/generated/CreateInstanceResponse';
import type { CreateLoaderBuildsView } from '../src/generated/CreateLoaderBuildsView';
import type { CreateVersionRow } from '../src/generated/CreateVersionRow';
import type { EnrichedInstance } from '../src/generated/EnrichedInstance';
import type { InstallQueueStateResponse } from '../src/generated/InstallQueueStateResponse';

function created(overrides: Partial<CreateInstanceResponse> = {}): CreateInstanceResponse {
  return {
    ...enrichedInstance({
      id: 'instance-7',
      name: 'My instance',
      version_id: '1.21.1',
      launchable: false,
      launch_action: {
        state_id: 'needs_install',
        label: 'Install',
        tone: 'mute',
        launchable: false,
        primary_action: 'install',
      },
    }),
    view_model: {
      state_id: 'created',
      tone: 'ok',
      title: 'Created',
      summary: 'Instance created.',
      detail: 'Download queued.',
    },
    ...overrides,
  };
}

function harness(options: { response?: CreateInstanceResponse; requestError?: Error; queueError?: Error } = {}) {
  const requests: unknown[][] = [];
  const added: EnrichedInstance[] = [];
  const queues: unknown[][] = [];
  const notices: unknown[][] = [];
  const navigation: unknown[] = [];
  const instances = { value: added as EnrichedInstance[] };
  const call = async (method: string, path: string, body?: unknown) => {
    requests.push(structuredClone([method, path, body]));
    if (options.requestError) throw options.requestError;
    return structuredClone(options.response ?? created());
  };
  const create = loadSource<typeof import('../src/create/create-instance')>('create/create-instance.ts', {
    'app/toast': {
      toast: (...args: unknown[]) => notices.push(structuredClone(args)),
      errMessage: (error: unknown) => (error instanceof Error ? error.message : String(error)),
    },
    'app/route': { navigate: (value: unknown) => navigation.push(structuredClone(value)) },
    'instances/state': {
      instances,
      addInstance: (value: EnrichedInstance) => {
        added.push(value);
      },
    },
    'install/queue': {
      async applyInstallQueueResponse(...args: unknown[]) {
        queues.push(structuredClone(args));
        if (options.queueError) throw options.queueError;
      },
      async refreshInstallQueue() {},
    },
    'create/presenters': presenters,
    'create/api': apiModule('create/api.ts', call),
  });
  return { create, requests, added, queues, notices, navigation };
}

const args = { name: '  My instance  ', selectionId: 'minecraft|1.21.1', icon: 'stack', accent: 'verdant' };

test('create sends effective defaults and preserves the backend-authored launch action', async () => {
  const h = harness();
  const initialSettings = {
    max_memory_mb: 6144,
    art_seed: 108,
    window_width: 1280,
    window_height: 720,
    jvm_preset_id: 'balanced',
    auto_optimize: true,
  };
  const result = await h.create.createInstance({ ...args, initialSettings });
  assert.equal(result.ok, true);
  assert.deepEqual(h.requests, [
    [
      'POST',
      '/instances',
      { name: 'My instance', selection_id: 'minecraft|1.21.1', icon: 'stack', accent: 'verdant', ...initialSettings },
    ],
  ]);
  assert.deepEqual(result.instance?.launch_action, created().launch_action);
  assert.equal(h.added.length, 1);
  assert.deepEqual(h.navigation, [{ name: 'instance', id: 'instance-7' }]);
  assert.deepEqual(h.notices, [['Instance created. Download queued.', 'success']]);
});

test('blank identity drafts are rejected before any mutation', async () => {
  const h = harness();
  assert.deepEqual(structuredClone(await h.create.createInstance({ ...args, name: ' ' })), {
    ok: false,
    error: 'Name is required',
  });
  assert.deepEqual(structuredClone(await h.create.createInstance({ ...args, selectionId: '' })), {
    ok: false,
    error: 'Version is required',
  });
  assert.deepEqual(h.requests, []);
  assert.deepEqual(h.added, []);
});

test('staged setup commits the exact opaque plan and selection', async () => {
  const h = harness();
  await h.create.createInstance({ ...args, selectionId: 'resolved|build+opaque', setupPlanId: 'setup-plan-8' });
  assert.deepEqual(h.requests, [
    [
      'POST',
      '/instances/setup',
      {
        plan_id: 'setup-plan-8',
        name: 'My instance',
        selection_id: 'resolved|build+opaque',
        icon: 'stack',
        accent: 'verdant',
      },
    ],
  ]);
});

test('pack handoff keeps provider identity and exact version', async () => {
  const h = harness();
  await h.create.createInstance({ ...args, modpack: { canonicalId: 'modrinth:opaque/pack', versionId: 'release+7' } });
  assert.deepEqual(h.requests, [
    [
      'POST',
      '/instances/modpack',
      {
        canonical_id: 'modrinth:opaque/pack',
        version_id: 'release+7',
        name: 'My instance',
        selection_id: 'minecraft|1.21.1',
        icon: 'stack',
        accent: 'verdant',
      },
    ],
  ]);
});

test('a refused or failed create does not publish local success or retry the mutation', async () => {
  for (const requestError of [new Error('Target is busy'), new Error('Connection lost')]) {
    const h = harness({ requestError });
    const result = await h.create.createInstance(args);
    assert.equal(result.ok, false);
    assert.equal(h.requests.length, 1);
    assert.deepEqual(h.added, []);
    assert.deepEqual(h.navigation, []);
    assert.equal(h.notices[0]?.[1], 'error');
  }
});

test('confirmed creation passes the authoritative queue snapshot to its owner', async () => {
  const snapshot: InstallQueueStateResponse = queueSnapshot({ revision: 4 });
  const h = harness({ response: created({ install_queue: snapshot }) });
  const result = await h.create.createInstance(args);
  assert.equal(result.ok, true);
  assert.deepEqual(h.queues, [[snapshot, { connectActive: true }]]);
});

test('queue connection failure cannot reopen an already completed creation', async () => {
  const h = harness({
    response: created({ install_queue: queueSnapshot() }),
    queueError: new Error('Stream unavailable'),
  });
  const result = await h.create.createInstance(args);
  assert.equal(result.ok, true);
  assert.equal(h.requests.length, 1);
  assert.equal(h.added.length, 1);
  assert.deepEqual(h.navigation, [{ name: 'instance', id: 'instance-7' }]);
  assert.deepEqual(h.notices[h.notices.length - 1], [
    'Instance created, but download status could not be refreshed: Stream unavailable',
    'error',
  ]);
});

function row(id: string, channel: CreateVersionRow['channel']): CreateVersionRow {
  return {
    source_id: 'vanilla',
    selection_id: `vanilla|${id}`,
    minecraft_version_id: id,
    display_name: id,
    hint: null,
    channel,
    download_state: 'download',
    create_enabled: true,
    disabled_reason: null,
  };
}

test('lifecycle channels open on Release and retain Snapshot, Legacy and Unknown filters', () => {
  const rows = (
    [
      ['1.21.4', 'stable'],
      ['25w14a', 'preview'],
      ['1.18-experimental-snapshot-1', 'experimental'],
      ['a1.2.6', 'legacy'],
      ['unclassified', 'unknown'],
    ] as const
  ).map(([id, channel]) => versionRow(row(id, channel)));
  assert.equal(normalizeChannel('stable'), 'release');
  assert.deepEqual(
    rows.map((entry) => entry.channel),
    ['release', 'snapshot', 'snapshot', 'legacy', 'unknown'],
  );
  assert.equal(rows[0]?.downloadState, 'none');
  assert.deepEqual(
    CHANNEL_ORDER.map((channel) => CHANNEL_LABEL[channel]),
    ['Release', 'Snapshot', 'Legacy', 'Other'],
  );
});

test('unknown lifecycle data stays Unknown even when a version name looks like a release', () => {
  assert.equal(versionRow(row('1.21.4', 'unknown')).channel, 'unknown');
  assert.equal(normalizeChannel('future-channel'), 'unknown');
  assert.equal(normalizeChannel(undefined), 'unknown');
});

function builds(overrides: Partial<CreateLoaderBuildsView> = {}): CreateLoaderBuildsView {
  return {
    source_id: 'net.fabricmc.fabric-loader',
    minecraft_version_id: '1.21.1',
    auto: {
      selection_id: 'backend-auto+opaque',
      label: 'Automatic',
      detail: 'Backend automatic detail',
      enabled: true,
      disabled_reason: null,
    },
    builds: [
      {
        selection_id: 'pin+accepted',
        build_id: 'opaque/build',
        label: 'Backend stable label',
        channel_id: 'stable',
        channel_label: 'Stable',
        recommended: true,
        installed: false,
        enabled: true,
        disabled_reason: null,
      },
      {
        selection_id: 'pin+disabled',
        build_id: 'opaque/disabled',
        label: 'Backend disabled label',
        channel_id: 'beta',
        channel_label: 'Beta',
        recommended: false,
        installed: true,
        enabled: false,
        disabled_reason: 'Unsupported Java',
      },
    ],
    ...overrides,
  };
}

test('loader choice uses backend Automatic, rejects disabled pins and discards stale target pins', () => {
  const parsed = builds();
  const fabric: CreateSourceId = 'net.fabricmc.fabric-loader';
  const select = (choice: string | null, source: CreateSourceId = fabric, minecraft = parsed.minecraft_version_id) =>
    createLoaderSelection(parsed, source, minecraft, 'row-auto', choice);
  assert.equal(select(null), 'backend-auto+opaque');
  assert.equal(select('pin+accepted'), 'pin+accepted');
  assert.equal(select('pin+disabled'), '');
  assert.equal(select('unrecognized-pin'), '');
  assert.equal(select('pin+accepted', 'net.minecraftforge'), '');
  assert.equal(select('pin+accepted', fabric, '1.20.1'), '');
  assert.equal(select('pin+accepted', 'vanilla'), 'row-auto');
  assert.equal(createLoaderSelection(null, fabric, '1.21.1', 'row-auto', null), '');
  assert.equal(createLoaderSelection(null, 'vanilla', '1.21.1', 'row-auto', null), 'row-auto');
});

test('stale preferred build requires explicitly choosing the available older installed build', () => {
  const parsed = builds({
    auto: {
      selection_id: 'loader_auto|net.fabricmc.fabric-loader|1.21.1',
      label: 'Automatic',
      detail: 'Use the preferred compatible loader build.',
      enabled: false,
      disabled_reason: 'Refresh the loader catalog before installing the automatic build.',
    },
    builds: [
      {
        selection_id: 'loader_build|net.fabricmc.fabric-loader|preferred-build',
        build_id: 'preferred-build',
        label: '0.16.10',
        channel_id: 'stable',
        channel_label: 'Stable',
        recommended: true,
        installed: false,
        enabled: false,
        disabled_reason: 'Refresh the loader catalog before installing this build.',
      },
      {
        selection_id: 'loader_build|net.fabricmc.fabric-loader|older-build',
        build_id: 'older-build',
        label: '0.16.9',
        channel_id: 'stable',
        channel_label: 'Stable',
        recommended: false,
        installed: true,
        enabled: true,
        disabled_reason: null,
      },
    ],
  });
  const select = (choice: string | null) =>
    createLoaderSelection(parsed, 'net.fabricmc.fabric-loader', '1.21.1', parsed.auto.selection_id, choice);
  assert.equal(select(null), '');
  assert.equal(select(parsed.builds[0]!.selection_id), '');
  assert.equal(select(parsed.builds[1]!.selection_id), parsed.builds[1]!.selection_id);
});

function searchRow(sourceId: CreateSourceId, id: string, channel: Channel = 'release'): VersionRowModel {
  return {
    id,
    sourceId,
    selectionId: `${sourceId}|${id}`,
    displayName: id,
    hint: null,
    channel,
    downloadState: 'none',
    createEnabled: true,
    disabledReason: null,
  };
}

test('a typed loader and version resolve to the loader that has that version', () => {
  const offered: LoaderKey[] = ['vanilla', 'fabric', 'quilt', 'forge', 'neoforge'];
  assert.deepEqual(parseVersionQuery('f1131', offered), { loader: 'forge', version: '1131' });
  assert.deepEqual(parseVersionQuery('fa1211', offered), { loader: 'fabric', version: '1211' });
  assert.deepEqual(parseVersionQuery('n1211', offered), { loader: 'neoforge', version: '1211' });
  assert.deepEqual(parseVersionQuery(' Forge 1.13.1 ', offered), { loader: 'forge', version: '1.13.1' });
  assert.deepEqual(parseVersionQuery('1.13.1 fo', offered), { loader: 'forge', version: '1.13.1' });
  assert.deepEqual(parseVersionQuery('b1.7.3', offered), { loader: null, version: 'b1.7.3' });
  assert.deepEqual(parseVersionQuery('1.21 pre', offered), { loader: null, version: '1.21 pre' });
  assert.deepEqual(parseVersionQuery('f1131', ['vanilla']), { loader: null, version: 'f1131' });

  const rows = [
    ...['1.21.11', '1.21.1', '1.14.4'].map((id) => searchRow('net.fabricmc.fabric-loader', id)),
    ...['1.21.1', '1.13.2', '1.13.1', '1.1'].map((id) => searchRow('net.minecraftforge', id)),
  ];
  assert.equal(rankVersions(rows, '1131')[0]?.selectionId, 'net.minecraftforge|1.13.1');
  assert.deepEqual(
    rankVersions(rows, '1211').map((entry) => entry.selectionId),
    ['net.fabricmc.fabric-loader|1.21.1', 'net.minecraftforge|1.21.1', 'net.fabricmc.fabric-loader|1.21.11'],
  );
});

test('version search reaches every channel and keeps releases ahead of equal matches', () => {
  const rows = [
    searchRow('vanilla', '1.21.5-rc1', 'snapshot'),
    searchRow('vanilla', '1.21.5'),
    searchRow('vanilla', '1.21'),
    searchRow('vanilla', '25w14a', 'snapshot'),
    searchRow('vanilla', '1.2.1', 'legacy'),
  ];
  assert.deepEqual(
    rankVersions(rows, '1.21').map((entry) => entry.id),
    ['1.21', '1.21.5', '1.21.5-rc1'],
  );
  assert.deepEqual(
    rankVersions(rows, '121').map((entry) => entry.id),
    ['1.21', '1.2.1', '1.21.5', '1.21.5-rc1'],
  );
  assert.deepEqual(
    rankVersions(rows, '25w').map((entry) => entry.id),
    ['25w14a'],
  );
  assert.equal(rankVersions(rows, '').length, rows.length);
});
