import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';

const frontend = basename(process.cwd()) === 'frontend' ? process.cwd() : resolve(process.cwd(), 'frontend');

// The replacement installs its own dependencies during composition. The
// preserved toolchain may be read for an isolated, entirely in-memory check.
/** @type {typeof import('esbuild').build} */
let build;
try {
  ({ build } = createRequire(resolve(frontend, 'package.json'))('esbuild'));
} catch (error) {
  if (!(error instanceof Error) || !('code' in error) || error.code !== 'MODULE_NOT_FOUND') throw error;
  ({ build } = createRequire(resolve(frontend, '../legacy/frontend/package.json'))('esbuild'));
}

const instanceFile = resolve(frontend, 'src/instance-create.ts');
const seam = `
  export const requests = [], added = [], queues = [], notices = [], navigation = [];
  export const instances = { value: added };
  let response, requestError, queueError;
  export function arrange(value, failure, streamFailure) {
    response = value; requestError = failure; queueError = streamFailure;
    for (const values of [requests, added, queues, notices, navigation]) values.length = 0;
  }
  export async function api(...args) {
    requests.push(args);
    if (requestError) throw requestError;
    return structuredClone(response);
  }
  export function addInstance(value) { added.push(value); }
  export async function applyInstallQueueResponse(...args) {
    queues.push(args);
    if (queueError) throw queueError;
  }
  export async function refreshInstallQueue() {}
  export function toast(...args) { notices.push(args); }
  export function navigate(value) { navigation.push(value); }
  export function errMessage(value) { return value instanceof Error ? value.message : String(value); }
`;
const bundled = await build({
  stdin: {
    contents: `
      export * from './src/instance-create';
      export * from './src/create-presenters';
      export * from './src/views/create/contract';
      export * from './src/views/create/view-model';
      export * from 'create-test-seam';
    `,
    resolveDir: frontend,
  },
  bundle: true,
  write: false,
  platform: 'node',
  format: 'esm',
  logLevel: 'silent',
  plugins: [
    {
      name: 'create-side-effects',
      setup(plugin) {
        plugin.onResolve({ filter: /.*/ }, (args) => {
          if (
            args.path === 'create-test-seam' ||
            (args.importer === instanceFile &&
              ['./api', './toast', './utils', './ui-state', './actions', './store', './machines/downloads'].includes(
                args.path,
              ))
          ) {
            return { path: 'create-test-seam', namespace: 'create-test' };
          }
        });
        plugin.onLoad({ filter: /.*/, namespace: 'create-test' }, () => ({ contents: seam, loader: 'js' }));
      },
    },
  ],
});
/**
 * @typedef {typeof import('../../src/instance-create') & typeof import('../../src/create-presenters') &
 * typeof import('../../src/views/create/contract') & typeof import('../../src/views/create/view-model') & {
 * arrange: (response: unknown, failure?: unknown, streamFailure?: unknown) => void,
 * requests: Parameters<typeof import('../../src/api').api>[],
 * added: Parameters<typeof import('../../src/actions').addInstance>[0][],
 * queues: Parameters<typeof import('../../src/machines/downloads').applyInstallQueueResponse>[],
 * notices: Parameters<typeof import('../../src/toast').toast>[],
 * navigation: Parameters<typeof import('../../src/ui-state').navigate>[0][]
 * }} CreateTestModule
 */
/** @type {CreateTestModule} */
const ui = await import(`data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString('base64')}`);

function created() {
  return {
    id: 'instance-7',
    name: 'My instance',
    version_id: '1.21.1',
    created_at: '2026-09-08T00:00:00Z',
    java_selection: { kind: 'inherited' },
    revision: 1,
    version_display: {
      loader_key: 'vanilla',
      loader_label: 'Vanilla',
      minecraft_label: '1.21.1',
      loader_version_label: '',
      loader_detail_label: '',
      summary_label: 'Minecraft 1.21.1',
      supports_mods: false,
    },
    launchable: false,
    launch_action: {
      state_id: 'needs_install',
      label: 'Install',
      tone: 'mute',
      launchable: false,
      primary_action: 'install',
    },
    saves_count: 0,
    mods_count: 0,
    resource_count: 0,
    shader_count: 0,
    view_model: { state_id: 'created', tone: 'success', summary: 'Instance created.', detail: 'Download queued.' },
  };
}

function queue() {
  return {
    queue_epoch: 'queue-1',
    revision: 4,
    registry_revision: 0,
    latest_failure: null,
    active: null,
    items: [],
    view_model: {
      state_id: 'idle',
      status_label: 'Idle',
      title: 'Downloads',
      summary: 'No active download',
      queued_count: 0,
      queued_count_label: '0 queued',
      queued_item_label: 'Queued',
      next_label: null,
      active_queued_count_label: null,
      section_title: 'Queue',
      empty_title: 'No downloads',
      empty_summary: '',
    },
    notice: null,
    started_install: null,
    removed_instance_id: null,
  };
}

const args = { name: '  My instance  ', selectionId: 'minecraft|1.21.1', icon: 'stack', accent: 'verdant' };

test('create sends effective defaults and preserves the backend-authored launch action', async () => {
  ui.arrange(created());
  const initialSettings = {
    max_memory_mb: 6144,
    art_seed: 108,
    window_width: 1280,
    window_height: 720,
    jvm_preset_id: 'balanced',
    auto_optimize: true,
  };
  const result = await ui.createInstance({ ...args, initialSettings });
  assert.equal(result.ok, true);
  assert.ok(result.instance);
  assert.deepEqual(ui.requests, [
    [
      'POST',
      '/instances',
      {
        name: 'My instance',
        selection_id: 'minecraft|1.21.1',
        icon: 'stack',
        accent: 'verdant',
        ...initialSettings,
      },
    ],
  ]);
  assert.deepEqual(result.instance.launch_action, { ...created().launch_action, disabled_reason: undefined });
  assert.equal(ui.added.length, 1);
  assert.deepEqual(ui.navigation, [{ name: 'instance', id: 'instance-7' }]);
  assert.deepEqual(ui.notices, [['Instance created. Download queued.', 'success']]);
});

test('blank identity drafts are rejected before any mutation', async () => {
  ui.arrange(created());
  assert.deepEqual(await ui.createInstance({ ...args, name: ' ' }), { ok: false, error: 'Name is required' });
  assert.deepEqual(await ui.createInstance({ ...args, selectionId: '' }), { ok: false, error: 'Version is required' });
  assert.deepEqual(ui.requests, []);
  assert.deepEqual(ui.added, []);
});

test('staged setup commits the exact opaque plan and selection', async () => {
  ui.arrange(created());
  await ui.createInstance({ ...args, selectionId: 'resolved|build+opaque', setupPlanId: 'setup-plan-8' });
  assert.deepEqual(ui.requests, [
    [
      'POST',
      '/instances/setup',
      {
        name: 'My instance',
        selection_id: 'resolved|build+opaque',
        icon: 'stack',
        accent: 'verdant',
        plan_id: 'setup-plan-8',
      },
    ],
  ]);
});

test('pack handoff keeps provider identity and exact version', async () => {
  ui.arrange(created());
  await ui.createInstance({ ...args, modpack: { canonicalId: 'modrinth:opaque/pack', versionId: 'release+7' } });
  assert.deepEqual(ui.requests, [
    [
      'POST',
      '/instances/modpack',
      {
        name: 'My instance',
        selection_id: 'minecraft|1.21.1',
        icon: 'stack',
        accent: 'verdant',
        canonical_id: 'modrinth:opaque/pack',
        version_id: 'release+7',
      },
    ],
  ]);
});

for (const [label, response, failure] of [
  ['backend refusal', { error: 'Target is busy' }],
  ['transport failure', undefined, new Error('Connection lost')],
  ['invalid identity', { ...created(), id: 7 }],
  ['missing launch authority', { ...created(), launch_action: undefined }],
  ['missing result presentation', { ...created(), view_model: undefined }],
]) {
  test(`${label} does not publish local success or retry the mutation`, async () => {
    ui.arrange(response, failure);
    const result = await ui.createInstance(args);
    assert.equal(result.ok, false);
    assert.equal(ui.requests.length, 1);
    assert.deepEqual(ui.added, []);
    assert.deepEqual(ui.navigation, []);
    assert.equal(ui.notices[0][1], 'error');
  });
}

test('confirmed creation passes the authoritative queue snapshot to its owner', async () => {
  const snapshot = queue();
  ui.arrange({ ...created(), install_queue: snapshot });
  const result = await ui.createInstance(args);
  assert.equal(result.ok, true);
  assert.deepEqual(ui.queues, [[snapshot, { connectActive: true }]]);
});

test('queue connection failure cannot reopen an already completed creation', async () => {
  ui.arrange({ ...created(), install_queue: queue() }, undefined, new Error('Stream unavailable'));
  const result = await ui.createInstance(args);
  assert.equal(result.ok, true);
  assert.equal(ui.requests.length, 1);
  assert.equal(ui.added.length, 1);
  assert.deepEqual(ui.navigation, [{ name: 'instance', id: 'instance-7' }]);
  assert.deepEqual(ui.notices[ui.notices.length - 1], [
    'Instance created, but download status could not be refreshed: Stream unavailable',
    'error',
  ]);
});

test('malformed queue data preserves confirmed creation without publishing invalid queue state', async () => {
  ui.arrange({ ...created(), install_queue: { ...queue(), revision: 'invalid' } });
  const result = await ui.createInstance(args);
  assert.equal(result.ok, true);
  assert.equal(ui.requests.length, 1);
  assert.equal(ui.added.length, 1);
  assert.deepEqual(ui.queues, []);
  assert.deepEqual(ui.navigation, [{ name: 'instance', id: 'instance-7' }]);
  const notice = ui.notices[ui.notices.length - 1];
  assert.ok(notice);
  assert.match(notice[0], /download status could not be refreshed/);
  assert.equal(notice[1], 'error');
});

test('Guardian-only response data has no presentation authority', async () => {
  ui.arrange({ ...created(), guardian_notice: { tone: 'error', message: 'Old autonomous repair' } });
  assert.equal((await ui.createInstance(args)).ok, true);
  assert.deepEqual(ui.notices, [['Instance created. Download queued.', 'success']]);
  assert.equal(ui.createResultToastMessage({ view_model: { summary: 'Queued.', detail: 'Queued.' } }), 'Queued.');
  assert.equal(ui.createToastKind('warn'), 'info');
  assert.deepEqual(ui.createNoticePresentation({ state_id: 'runtime', tone: 'warn', message: 'Select Java' }), {
    tone: 'warned',
    icon: 'alert',
  });
});

function view() {
  return {
    sources: [
      'vanilla',
      'net.fabricmc.fabric-loader',
      'org.quiltmc.quilt-loader',
      'net.minecraftforge',
      'net.neoforged',
    ].map((id) => ({ id, label: `Backend ${id}`, enabled: true })),
    channels: [{ id: 'release', label: 'Release', enabled: true }],
    versions: [
      {
        source_id: 'vanilla',
        selection_id: 'opaque-selection-7',
        minecraft_version_id: '1.21.1',
        display_name: 'Backend version label',
        channel: 'release',
        download_state: 'base',
        create_enabled: false,
        disabled_reason: 'Runtime unavailable',
        tags: [{ id: 'newest', label: 'Latest' }],
      },
    ],
    preset_options: [{ id: 'balanced', label: 'Balanced', detail: 'Backend Performance detail', default: true }],
    optimize_option: {
      id: 'optimize',
      label: 'Backend auto-optimize',
      detail: 'Backend optimization detail',
      default_enabled: true,
    },
    notices: [{ state_id: 'runtime', tone: 'warn', message: 'Backend notice', detail: null }],
    defaults: {
      source_id: 'vanilla',
      channel_id: 'release',
      jvm_preset_id: 'balanced',
      max_memory_mb: 6144,
      window_width: 1280,
      window_height: 720,
    },
  };
}

test('create options retain every loader, backend labels, disabled reasons, defaults and Performance', () => {
  const parsed = ui.createBackendViewResponse(view());
  assert.equal(parsed.sources.length, 5);
  assert.deepEqual(parsed.defaults, view().defaults);
  assert.deepEqual(parsed.optimize_option, view().optimize_option);
  assert.equal(parsed.versions[0].selection_id, 'opaque-selection-7');
  assert.equal(parsed.versions[0].create_enabled, false);
  assert.equal(parsed.versions[0].disabled_reason, 'Runtime unavailable');
  assert.equal(parsed.versions[0].display_name, 'Backend version label');
});

test('create notices preserve omitted detail and backend guidance', () => {
  const response = {
    ...view(),
    notices: [
      { state_id: 'catalog_unavailable', tone: 'warn', message: 'Catalog unavailable' },
      {
        state_id: 'library_scan_degraded',
        tone: 'warn',
        message: 'Installed versions are unavailable',
        detail: 'Could not verify installed versions. Check the library folder and try again.',
      },
    ],
  };
  const parsed = ui.createBackendViewResponse(response);
  assert.equal(parsed.notices[0].detail, undefined);
  assert.equal(
    parsed.notices[1].detail,
    'Could not verify installed versions. Check the library folder and try again.',
  );
});

function lifecycleView() {
  // Current SetupService::create_view wire values, including the five lifecycle
  // channels, stable default and no legacy-only tags/build attachment fields.
  return {
    sources: [{ id: 'vanilla', label: 'Vanilla', enabled: true }],
    channels: [
      ['stable', 'Stable'],
      ['preview', 'Preview'],
      ['experimental', 'Experimental'],
      ['legacy', 'Legacy'],
      ['unknown', 'Other'],
    ].map(([id, label]) => ({ id, label, enabled: true })),
    versions: [
      ['1.21.4', 'stable'],
      ['25w14a', 'preview'],
      ['1.18-experimental-snapshot-1', 'experimental'],
      ['a1.2.6', 'legacy'],
      ['unclassified', 'unknown'],
    ].map(([id, channel]) => ({
      source_id: 'vanilla',
      selection_id: `vanilla|${id}`,
      minecraft_version_id: id,
      display_name: id,
      hint: null,
      channel,
      download_state: 'download',
      create_enabled: true,
      disabled_reason: null,
    })),
    preset_options: [
      { id: '', label: 'Automatic', detail: 'Choose a preset for this Minecraft and Java version.', default: true },
    ],
    optimize_option: {
      id: 'auto_optimize',
      label: 'Optimize automatically',
      detail: 'Apply the selected Performance settings when the instance is prepared.',
      default_enabled: true,
    },
    notices: [],
    defaults: {
      source_id: 'vanilla',
      channel_id: 'stable',
      jvm_preset_id: '',
      max_memory_mb: 4096,
      window_width: 1280,
      window_height: 720,
    },
  };
}

test('actual lifecycle create response opens on Release and retains Snapshot, Legacy and Unknown filters', () => {
  const parsed = ui.createBackendViewResponse(lifecycleView());
  assert.equal(parsed.defaults.channel_id, 'release');
  assert.deepEqual(
    parsed.versions.map((row) => row.channel),
    ['release', 'snapshot', 'snapshot', 'legacy', 'unknown'],
  );
  assert.deepEqual(
    parsed.versions.filter((row) => row.channel === parsed.defaults.channel_id).map((row) => row.minecraft_version_id),
    ['1.21.4'],
  );
  assert.deepEqual(
    parsed.versions.filter((row) => row.channel === 'snapshot').map((row) => row.minecraft_version_id),
    ['25w14a', '1.18-experimental-snapshot-1'],
  );
  assert.deepEqual(
    [...new Set(parsed.channels.map((option) => option.id))],
    ['release', 'snapshot', 'legacy', 'unknown'],
  );
  assert.deepEqual(
    ui.CHANNEL_ORDER.map((channel) => ui.CHANNEL_LABEL[channel]),
    ['Release', 'Snapshot', 'Legacy', 'Unknown'],
  );
});

test('unknown lifecycle data stays Unknown even when a version name looks like a release', () => {
  const wire = lifecycleView();
  const parsed = ui.createBackendViewResponse({
    ...wire,
    defaults: { ...wire.defaults, channel_id: 'unknown' },
    versions: [
      { ...wire.versions[0], channel: 'unknown' },
      { ...wire.versions[0], channel: 'future-channel' },
    ],
  });
  assert.equal(parsed.defaults.channel_id, 'unknown');
  assert.deepEqual(
    parsed.versions.map((row) => row.channel),
    ['unknown', 'unknown'],
  );
  assert.equal(
    parsed.versions.every((row) => row.minecraft_version_id === '1.21.4'),
    true,
  );
  assert.throws(
    () =>
      ui.createBackendViewResponse({
        ...wire,
        versions: [{ ...wire.versions[0], channel: 42 }],
      }),
    /Create version channel/,
  );
});

test('malformed availability and unknown sources fail at the transport boundary', () => {
  const valid = view();
  assert.throws(
    () => ui.createBackendViewResponse({ ...valid, versions: [{ ...valid.versions[0], tags: [{ id: 'latest' }] }] }),
    /Create version tag label/,
  );
  assert.throws(
    () => ui.createBackendViewResponse({ ...valid, versions: [{ ...valid.versions[0], create_enabled: 'yes' }] }),
    /Create version enabled/,
  );
  assert.throws(
    () =>
      ui.createBackendViewResponse({ ...valid, sources: [{ id: 'unknown-loader', label: 'Unknown', enabled: true }] }),
    /Create source id/,
  );
  assert.throws(
    () =>
      ui.createBackendViewResponse({ ...valid, optimize_option: { ...valid.optimize_option, default_enabled: 'yes' } }),
    /Create optimize default/,
  );
});

function builds() {
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
  };
}

test('loader options bind exact source and Minecraft identity and preserve opaque builds', () => {
  const parsed = ui.createLoaderBuildsResponse(builds(), {
    sourceId: 'net.fabricmc.fabric-loader',
    minecraftVersionId: '1.21.1',
  });
  assert.equal(parsed.builds[0].build_id, 'opaque/build');
  assert.equal(parsed.builds[0].label, 'Backend stable label');
  assert.equal(parsed.builds[1].disabled_reason, 'Unsupported Java');
  /** @type {NonNullable<Parameters<typeof ui.createLoaderBuildsResponse>[1]>[]} */
  const mismatches = [
    { sourceId: 'net.minecraftforge', minecraftVersionId: '1.21.1' },
    { sourceId: 'net.fabricmc.fabric-loader', minecraftVersionId: '1.20.1' },
  ];
  for (const expected of mismatches)
    assert.throws(() => ui.createLoaderBuildsResponse(builds(), expected), /did not match/);
});

test('loader choice uses backend Automatic, rejects disabled pins and discards stale target pins', () => {
  const parsed = ui.createLoaderBuildsResponse(builds());
  /** @type {import('../../src/views/create/contract').CreateSourceId} */
  const parsedSource = 'net.fabricmc.fabric-loader';
  assert.equal(parsed.source_id, parsedSource);
  /** @param {string | null} choice @param {import('../../src/views/create/contract').CreateSourceId} [source] */
  const select = (choice, source = parsedSource, minecraft = parsed.minecraft_version_id) =>
    ui.createLoaderSelection(parsed, source, minecraft, 'row-auto', choice);
  assert.equal(select(null), 'backend-auto+opaque');
  assert.equal(select('pin+accepted'), 'pin+accepted');
  assert.equal(select('pin+disabled'), '');
  assert.equal(select('unrecognized-pin'), '');
  assert.equal(select('pin+accepted', 'net.minecraftforge'), '');
  assert.equal(select('pin+accepted', parsedSource, '1.20.1'), '');
  assert.equal(select('pin+accepted', 'vanilla'), 'row-auto');
  assert.equal(ui.createLoaderSelection(null, parsedSource, '1.21.1', 'row-auto', null), '');
  assert.equal(ui.createLoaderSelection(null, 'vanilla', '1.21.1', 'row-auto', null), 'row-auto');
});

test('loader Automatic availability is required at the transport boundary', () => {
  const valid = builds();
  for (const enabled of [undefined, null, 'true']) {
    assert.throws(
      () => ui.createLoaderBuildsResponse({ ...valid, auto: { ...valid.auto, enabled } }),
      /Create loader automatic enabled/,
    );
  }
  assert.throws(
    () => ui.createLoaderBuildsResponse({ ...valid, auto: { ...valid.auto, disabled_reason: undefined } }),
    /Create loader automatic disabled reason/,
  );
});

test('stale preferred build requires explicitly choosing the available older installed build', () => {
  // Current Rust CreateLoaderBuildsView wire shape: the preferred build is
  // missing locally while an older compatible build is display-ready.
  const parsed = ui.createLoaderBuildsResponse({
    source_id: 'net.fabricmc.fabric-loader',
    minecraft_version_id: '1.21.1',
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
  /** @param {string | null} choice */
  const select = (choice) =>
    ui.createLoaderSelection(parsed, 'net.fabricmc.fabric-loader', '1.21.1', parsed.auto.selection_id, choice);
  assert.equal(parsed.auto.enabled, false);
  assert.equal(parsed.auto.disabled_reason, 'Refresh the loader catalog before installing the automatic build.');
  assert.equal(select(null), '');
  assert.equal(select(parsed.builds[0].selection_id), '');
  assert.equal(select(parsed.builds[1].selection_id), parsed.builds[1].selection_id);
  assert.equal(select(null), '');
});

test('create styling preserves the baseline except restored loader marks and their exact alignment', async () => {
  for (const filename of ['create.css', 'loader-logos.tsx']) {
    const [replacement, baseline] = await Promise.all([
      readFile(resolve(frontend, 'src/views/create', filename), 'utf8'),
      readFile(resolve(frontend, '../legacy/frontend/src/views/create', filename), 'utf8'),
    ]);
    const comparable =
      filename === 'create.css'
        ? replacement.replace(
            ".cp-cr-loader-mark[data-loader='quilt'] {\n  translate: 2.1% 2.1%;\n}\n.cp-cr-loader-mark[data-loader='forge'] {\n  translate: 0 2.1%;\n}\n",
            '',
          )
        : replacement
            .replace('vanilla_icon.svg', 'loader-base.svg')
            .replace('fabric_icon.svg', 'loader-grid.svg')
            .replace('neoforge_icon.svg', 'loader-orbit.svg')
            .replace('forge_icon.svg', 'loader-cross.svg')
            .replace('quilt_icon.svg', 'loader-diamonds.svg')
            .replace('      data-loader={loader}\n', '');
    assert.equal(comparable, baseline);
  }
});
